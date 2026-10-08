//! Herdr terminal admission fed by the real readers' frames, the canonical binding log and the row.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::services::discord::{mailbox_try_start_turn, make_shared_data_for_tests};
use crate::services::provider::cancel_token_claude_interrupt::HerdrTurnStart;
use crate::services::tui_prompt_dedupe::binding_events as events;
use serenity::all::{ChannelId, MessageId, UserId};
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;

const SESSION: &str = "herdr-session";

fn codex(kind: &str, turn: Option<&str>) -> String {
    let record =
        serde_json::json!({"type": "event_msg", "payload": {"type": kind, "turn_id": turn}});
    format!("{record}\n")
}

fn reply(text: &str) -> String {
    let record = serde_json::json!({"type": "response_item", "payload": {"type": "message",
        "role": "assistant", "content": [{"type": "output_text", "text": text}]}});
    format!("{record}\n")
}

fn claude(record: serde_json::Value, at: chrono::DateTime<chrono::Utc>) -> String {
    let mut record = record;
    record["timestamp"] = at.to_rfc3339().into();
    format!("{record}\n")
}

fn identity(path: &Path) -> (u64, u64) {
    let meta = std::fs::metadata(path).unwrap();
    (meta.dev(), meta.ino())
}

struct Case {
    shared: Arc<crate::services::discord::SharedData>,
    actor: Arc<CancelToken>,
    local: InflightTurnState,
    baseline: InflightTurnState,
    expected: InflightTurnIdentity,
    provider: ProviderKind,
    path: PathBuf,
    logical: String,
    channel: u64,
    root: PathBuf,
}

impl Case {
    /// A marked Herdr pane whose canonical Source is `body`, logged by this token's execution,
    /// with this token's turn begun at `start` (Codex) or at `submitted_at` (Claude).
    async fn new(
        root: &Path,
        n: u64,
        provider: ProviderKind,
        body: &str,
        start: u64,
        submitted_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Self {
        let channel = 5_340_340_000 + n;
        let logical = format!("AgentDesk-{}-admit-{n}", provider.as_str());
        let path = root.join(format!("{logical}.jsonl"));
        std::fs::write(&path, body).unwrap();
        let path = std::fs::canonicalize(path).unwrap();
        let marker = crate::services::tmux_common::session_temp_path(&logical, "host_kind");
        std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(&marker, "herdr").unwrap();
        let shared = make_shared_data_for_tests();
        let actor = Arc::new(CancelToken::new());
        let owner = HostedOwner {
            provider: provider.as_str().into(),
            discord_token_hash: shared.token_hash.clone(),
            channel_id: channel.to_string(),
            logical_key: logical.clone(),
            owner_node: "node".into(),
            runtime_root: root.display().to_string(),
        };
        let state = actor.prepare_herdr_interrupt(provider.clone(), &owner);
        actor.bind_unmanaged_session_name(&logical);
        let row = InflightTurnState::new(
            provider.clone(),
            channel,
            None,
            1,
            77_100,
            18,
            String::new(),
            None,
            None,
            None,
            None,
            0,
        );
        let mut case = Self {
            shared,
            actor,
            baseline: row.clone(),
            expected: InflightTurnIdentity::from_state(&row),
            local: row,
            provider,
            path: path.clone(),
            logical,
            channel,
            root: root.to_path_buf(),
        };
        case.publish(1, &case.nonce());
        assert!(state.record_turn_start(HerdrTurnStart {
            execution_nonce: case.nonce(),
            source: path.clone(),
            file: Some(identity(&path)),
            offset: start,
            submitted_at,
        }));
        case.local.turn_nonce = case.actor.turn_nonce().map(str::to_owned);
        save_inflight_state_in_root(&inflight_runtime_root().unwrap(), &case.local).unwrap();
        case.baseline = case.local.clone();
        case.expected = InflightTurnIdentity::from_state(&case.local);
        assert!(
            mailbox_try_start_turn(
                &case.shared,
                ChannelId::new(channel),
                case.actor.clone(),
                UserId::new(1),
                MessageId::new(77_100)
            )
            .await
        );
        case
    }

    fn nonce(&self) -> String {
        format!("{:032x}", self.channel)
    }

    /// Appends a canonical Source event for `path` logged by `nonce`.
    fn publish(&self, seq: u64, nonce: &str) {
        let (dev, ino) = identity(&self.path);
        let event = events::BindingEvent {
            seq,
            channel_id: self.channel,
            provider: self.provider.as_str().into(),
            tmux_session: self.logical.clone(),
            execution_nonce: Some(nonce.to_owned()),
            old: None,
            new: events::BindingTarget::Source(events::SourceId {
                session_id: SESSION.into(),
                path: self.path.clone(),
                dev,
                ino,
            }),
            cause: events::BindingCause::Startup,
            parent_hint: None,
            evidence: events::BindingEvidence {
                hook_event: Some("SessionStart".into()),
                received_at: chrono::Utc::now(),
            },
            committed_at: chrono::Utc::now(),
        };
        let log = self.root.join(events::BINDING_EVENTS_DIR);
        std::fs::create_dir_all(&log).unwrap();
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log.join(format!("{}.log", self.channel)))
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&event).unwrap()).unwrap();
    }

    /// The typed frame the provider's real Herdr reader sends, reading from `from`.
    fn read(&self, from: u64) -> StreamMessage {
        self.try_read(from).expect("a provider terminal")
    }

    fn try_read(&self, from: u64) -> Option<StreamMessage> {
        if self.provider == ProviderKind::Claude {
            let path = self.path.display().to_string();
            let read = crate::services::claude::herdr_turn::herdr_terminal_frame;
            return read(&path, from, &self.logical, &self.actor);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let (path, actor) = (self.path.clone(), self.actor.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        std::thread::spawn(move || {
            crate::services::codex_tui::rollout_tail::tail_rollout_file_from_offset(
                &path,
                from,
                Some(SESSION),
                tx,
                Some(actor),
                move || std::time::Instant::now() < deadline,
            )
        })
        .join()
        .unwrap()
        .unwrap();
        rx.try_iter()
            .find(|frame| matches!(frame, StreamMessage::CodexTuiTerminalDone { .. }))
    }

    async fn admit(&mut self, frame: StreamMessage) -> Option<NativeTerminalKind> {
        let actor = (self.shared.as_ref(), &self.actor);
        let admitted = self
            .local
            .admit_tui_terminal_frame(&mut self.baseline, &self.expected, true, actor, "", frame)
            .await;
        let (message, range, _, kind) = admitted.ok()?;
        assert!(
            kind.is_some(),
            "a Herdr terminal is admitted or refused, never a plain Done"
        );
        assert!(range.is_none(), "a Herdr terminal pins no range");
        assert!(matches!(message, StreamMessage::Done { .. }), "{message:?}");
        assert_eq!(
            self.durable_kind(),
            kind,
            "the admitted kind is the durable one"
        );
        kind
    }

    fn durable_kind(&self) -> Option<NativeTerminalKind> {
        let row =
            crate::services::discord::inflight::load_inflight_state(&self.provider, self.channel);
        row.expect("the row stays").tui_terminal_kind
    }
}

/// Rewrites a typed frame's claimed coordinates, as a forged or stale frame would carry them.
fn claim(
    mut frame: StreamMessage,
    start: Option<u64>,
    end: Option<u64>,
    kind: Option<NativeTerminalKind>,
) -> StreamMessage {
    match &mut frame {
        StreamMessage::CodexTuiTerminalDone {
            source_start,
            complete_record_end,
            kind: claimed,
            ..
        }
        | StreamMessage::ClaudeTuiTerminalDone {
            source_start,
            complete_record_end,
            kind: claimed,
            ..
        } => {
            *source_start = start.unwrap_or(*source_start);
            *complete_record_end = end.unwrap_or(*complete_record_end);
            *claimed = kind.unwrap_or(*claimed);
        }
        _ => unreachable!("a typed terminal"),
    }
    frame
}

fn env(
    root: &Path,
) -> (
    crate::config::TestEnvVarGuard,
    std::sync::MutexGuard<'static, ()>,
) {
    let env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root);
    let lock = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    events::set_test_root(Some(root));
    (env, lock)
}

/// A Codex terminal is admitted only as the first terminal of this token's own turn from its start;
/// an earlier tail, a range past it, a foreign or unnamed record or a forged kind commits nothing.
#[tokio::test(flavor = "current_thread")]
async fn a_herdr_codex_terminal_is_admitted_only_from_this_tokens_own_turn() {
    use NativeTerminalKind::{Aborted, Completed};
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path());
    let own =
        codex("task_started", Some("t1")) + &reply("answer") + &codex("task_complete", Some("t1"));
    let mut case = Case::new(temp.path(), 1, ProviderKind::Codex, &own, 0, None).await;
    let frame = case.read(0);
    assert_eq!(case.admit(frame).await, Some(Completed));
    let mut forged = Case::new(temp.path(), 2, ProviderKind::Codex, &own, 0, None).await;
    let frame = claim(forged.read(0), None, None, Some(Aborted));
    assert_eq!(forged.admit(frame).await, None, "a forged kind");
    assert_eq!(forged.durable_kind(), None);

    let aborted =
        codex("task_started", Some("t1")) + &reply("partial") + &codex("turn_aborted", Some("t1"));
    let mut case = Case::new(temp.path(), 3, ProviderKind::Codex, &aborted, 0, None).await;
    let frame = case.read(0);
    assert_eq!(case.admit(frame).await, Some(Aborted));

    // The previous turn's tail, read from the file start, against this token's later start.
    let previous = codex("task_started", Some("t0")) + &codex("task_complete", Some("t0"));
    let body = previous.clone() + &own;
    let mut case = Case::new(
        temp.path(),
        4,
        ProviderKind::Codex,
        &body,
        previous.len() as u64,
        None,
    )
    .await;
    assert_eq!(
        case.admit(case.read(0)).await,
        None,
        "an earlier turn's tail"
    );
    assert_eq!(case.durable_kind(), None);

    // A range carried past this turn's terminal into the next turn's head and terminal.
    let next =
        codex("task_started", Some("t2")) + &reply("next") + &codex("task_complete", Some("t2"));
    let body = own.clone() + &next;
    let mut case = Case::new(temp.path(), 5, ProviderKind::Codex, &body, 0, None).await;
    let past = claim(case.read(0), None, Some(body.len() as u64), None);
    assert_eq!(
        case.admit(past).await,
        None,
        "a range past the first terminal"
    );

    // Another turn's completion and an unnamed pair end nothing the reader would accept.
    for (n, body, kind) in [
        (
            6,
            codex("task_started", Some("t1")) + &reply("a") + &codex("task_complete", Some("t0")),
            Completed,
        ),
        (
            7,
            codex("task_started", None) + &reply("a") + &codex("turn_aborted", None),
            Aborted,
        ),
    ] {
        let mut case = Case::new(temp.path(), n, ProviderKind::Codex, &own, 0, None).await;
        let template = case.read(0);
        std::fs::write(&case.path, &body).unwrap();
        let forged = claim(template, Some(0), Some(body.len() as u64), Some(kind));
        assert_eq!(case.admit(forged).await, None, "{body}");
        assert_eq!(case.durable_kind(), None);
    }
}

/// A Claude terminal is admitted from the first record at or after this token's input, through
/// its own prompt: an earlier turn's terminal or a range into the next turn commits nothing.
#[tokio::test(flavor = "current_thread")]
async fn a_herdr_claude_terminal_is_admitted_only_from_this_tokens_own_prompt() {
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path());
    let input = chrono::Utc::now();
    let before = input - chrono::Duration::seconds(5);
    let after = input + chrono::Duration::seconds(1);
    let prompt = |text: &str| serde_json::json!({"type": "user", "message": {"role": "user", "content": text}});
    let result =
        |text: &str| serde_json::json!({"type": "result", "subtype": "success", "result": text});
    let previous = claude(prompt("old"), before) + &claude(result("old answer"), before);
    let own = claude(prompt("question"), after) + &claude(result("answer"), after);
    let next = claude(prompt("next"), after) + &claude(result("next answer"), after);
    let body = previous.clone() + &own + &next;
    let start = previous.len() as u64;
    let mut case = Case::new(
        temp.path(),
        11,
        ProviderKind::Claude,
        &body,
        start,
        Some(input),
    )
    .await;
    let frame = case.read(start);
    let past = claim(frame.clone(), None, Some(body.len() as u64), None);
    assert_eq!(case.admit(past).await, None, "a range into the next turn");
    let earlier = claim(case.read(0), Some(0), Some(previous.len() as u64), None);
    assert_eq!(
        case.admit(earlier).await,
        None,
        "an earlier turn's terminal"
    );
    assert_eq!(case.durable_kind(), None);
    assert_eq!(case.admit(frame).await, Some(NativeTerminalKind::Completed));
}

/// A range holding the next turn's head before its terminal, or a previous turn's terminal or output
/// before its start, ends nothing for reader or admission; metadata and input before it still pass.
#[tokio::test(flavor = "current_thread")]
async fn a_herdr_range_holding_another_turns_head_or_tail_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path());
    let at = chrono::Utc::now() + chrono::Duration::seconds(1);
    let prompt = |text: &str| {
        claude(
            serde_json::json!({"type": "user", "message": {"role": "user", "content": text}}),
            at,
        )
    };
    let result = claude(
        serde_json::json!({"type": "result", "subtype": "success", "result": "answer"}),
        at,
    );
    let earlier = serde_json::json!({"type": "assistant", "message": {"role": "assistant",
        "content": [{"type": "text", "text": "earlier"}]}});
    let own =
        codex("task_started", Some("t1")) + &reply("answer") + &codex("task_complete", Some("t1"));
    let shapes = [
        (
            21,
            ProviderKind::Codex,
            own.clone(),
            codex("task_started", Some("t1"))
                + &codex("task_started", Some("t2"))
                + &reply("answer")
                + &codex("task_complete", Some("t1")),
        ),
        (
            22,
            ProviderKind::Codex,
            own.clone(),
            codex("task_complete", Some("t0")) + &own,
        ),
        (
            25,
            ProviderKind::Codex,
            own.clone(),
            reply("earlier") + &own,
        ),
        (
            23,
            ProviderKind::Claude,
            prompt("q1") + &result,
            prompt("q1") + &prompt("q2") + &result,
        ),
        (
            26,
            ProviderKind::Claude,
            prompt("q1") + &result,
            claude(earlier, at) + &prompt("q1") + &result,
        ),
    ];
    for (n, provider, valid, mixed) in shapes {
        let input = (provider == ProviderKind::Claude).then(|| at - chrono::Duration::seconds(2));
        let mut case = Case::new(temp.path(), n, provider, &valid, 0, input).await;
        let template = case.read(0);
        std::fs::write(&case.path, &mixed).unwrap();
        assert!(case.try_read(0).is_none(), "{n}: the reader ends nothing");
        let forged = claim(template, Some(0), Some(mixed.len() as u64), None);
        assert_eq!(case.admit(forged).await, None, "{n}: {mixed}");
        assert_eq!(case.durable_kind(), None, "{n}");
    }

    let meta = serde_json::json!({"type": "session_meta", "payload": {"id": SESSION}});
    let usage = serde_json::json!({"type": "event_msg", "payload": {"type": "token_count"}});
    let input = serde_json::json!({"type": "response_item", "payload": {"type": "message",
        "role": "user", "content": [{"type": "input_text", "text": "question"}]}});
    let body = format!("{meta}\n{usage}\n{input}\n") + &own;
    let mut case = Case::new(temp.path(), 24, ProviderKind::Codex, &body, 0, None).await;
    let frame = case.read(0);
    assert_eq!(case.admit(frame).await, Some(NativeTerminalKind::Completed));
}

/// A frame from a stale actor, a replaced mailbox turn, another execution's Source, a replaced or
/// truncated transcript, or an unmarked pane commits nothing.
#[tokio::test(flavor = "current_thread")]
async fn a_herdr_terminal_whose_actor_or_source_moved_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path());
    let own =
        codex("task_started", Some("t1")) + &reply("answer") + &codex("task_complete", Some("t1"));
    for (n, change) in [
        "actor",
        "mailbox",
        "binding",
        "replaced",
        "truncated",
        "marker",
    ]
    .into_iter()
    .enumerate()
    {
        let mut case = Case::new(
            temp.path(),
            20 + n as u64,
            ProviderKind::Codex,
            &own,
            0,
            None,
        )
        .await;
        let frame = case.read(0);
        let channel = ChannelId::new(case.channel);
        match change {
            "actor" => case.actor = Arc::new(CancelToken::new()),
            "mailbox" => {
                let provider = case.provider.clone();
                crate::services::discord::mailbox_finish_turn(&case.shared, &provider, channel)
                    .await;
                let successor = Arc::new(CancelToken::new());
                assert!(
                    mailbox_try_start_turn(
                        &case.shared,
                        channel,
                        successor,
                        UserId::new(1),
                        MessageId::new(9)
                    )
                    .await
                );
            }
            "binding" => case.publish(2, "another-execution"),
            "replaced" => {
                let fresh = case.path.with_extension("fresh");
                std::fs::write(&fresh, &own).unwrap();
                std::fs::rename(&fresh, &case.path).unwrap();
            }
            "truncated" => {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&case.path)
                    .unwrap();
                file.set_len(own.len() as u64 - 2).unwrap();
            }
            _ => std::fs::remove_file(crate::services::tmux_common::session_temp_path(
                &case.logical,
                "host_kind",
            ))
            .unwrap(),
        }
        assert_eq!(case.admit(frame).await, None, "{change}");
        assert_eq!(case.durable_kind(), None, "{change}");
    }
}

/// A rebind of the pane's Source cannot land between the Source judgement and the kind's commit:
/// it waits for the admission and then sees the kind already committed.
#[tokio::test(flavor = "current_thread")]
async fn a_source_rebind_waits_for_the_herdr_admission_commit() {
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path());
    let own =
        codex("task_started", Some("t1")) + &reply("answer") + &codex("task_complete", Some("t1"));
    let mut case = Case::new(temp.path(), 30, ProviderKind::Codex, &own, 0, None).await;
    let frame = case.read(0);
    let (provider, channel, logical) = (case.provider.clone(), case.channel, case.logical.clone());
    let rebind = Arc::new(std::sync::Mutex::new(None));
    let slot = rebind.clone();
    super::test_hooks::after_source_check_once(
        channel,
        Box::new(move || {
            let (provider, logical) = (provider.clone(), logical.clone());
            let rebinder = std::thread::spawn(move || {
                crate::services::tmux_common::with_tmux_source_authority(&logical, |_| {
                    let row =
                        crate::services::discord::inflight::load_inflight_state(&provider, channel);
                    row.and_then(|row| row.tui_terminal_kind)
                })
            });
            std::thread::sleep(std::time::Duration::from_millis(300));
            *slot.lock().unwrap() = Some(rebinder);
        }),
    );
    assert_eq!(case.admit(frame).await, Some(NativeTerminalKind::Completed));
    let rebinder = rebind.lock().unwrap().take().expect("the hook ran");
    assert_eq!(
        rebinder.join().unwrap(),
        Some(NativeTerminalKind::Completed),
        "the rebind ran only after the commit"
    );
}

/// The admitted kind survives the bridge's later guarded saves; a successor row carries none and,
/// like every unadmitted row, serializes no kind field at all.
#[tokio::test(flavor = "current_thread")]
async fn an_admitted_kind_survives_guarded_saves_and_never_reaches_a_successor() {
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path());
    let aborted =
        codex("task_started", Some("t1")) + &reply("partial") + &codex("turn_aborted", Some("t1"));
    let mut case = Case::new(temp.path(), 40, ProviderKind::Codex, &aborted, 0, None).await;
    let path = inflight_state_path(
        &inflight_runtime_root().unwrap(),
        &case.provider,
        case.channel,
    );
    assert!(
        !String::from_utf8(std::fs::read(&path).unwrap())
            .unwrap()
            .contains("tui_terminal_kind")
    );
    assert_eq!(
        case.admit(case.read(0)).await,
        Some(NativeTerminalKind::Aborted)
    );
    case.local.full_response = "partial".into();
    let saved = crate::services::discord::inflight::save_inflight_state_if_matches_identity(
        &case.local,
        &case.expected,
        None,
    );
    assert_eq!(saved, GuardedSaveOutcome::Saved);
    assert_eq!(case.durable_kind(), Some(NativeTerminalKind::Aborted));
    let mut successor = InflightTurnState::new(
        case.provider.clone(),
        case.channel,
        None,
        1,
        77_200,
        18,
        String::new(),
        None,
        None,
        None,
        None,
        0,
    );
    successor.turn_nonce = Some("successor".into());
    assert_eq!(successor.tui_terminal_kind, None);
    let json = serde_json::to_string(&successor).unwrap();
    assert!(!json.contains("tui_terminal_kind"), "{json}");
}

/// With settlement off the real reader keeps the legacy plain Done for a Herdr token and its turn
/// row serializes exactly as before: no typed frame, no kind field.
#[tokio::test(flavor = "current_thread")]
async fn settlement_off_keeps_the_legacy_herdr_frame_and_row_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path());
    let own =
        codex("task_started", Some("t1")) + &reply("answer") + &codex("task_complete", Some("t1"));
    let case = Case::new(temp.path(), 50, ProviderKind::Codex, &own, 0, None).await;
    let (tx, rx) = std::sync::mpsc::channel();
    let (path, actor) = (case.path.clone(), case.actor.clone());
    let read = std::thread::spawn(move || {
        use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;
        HERDR_SETTLEMENT_OVERRIDE.set(false);
        crate::services::codex_tui::rollout_tail::tail_rollout_file_from_offset(
            &path,
            0,
            Some(SESSION),
            tx,
            Some(actor),
            || true,
        )
    });
    assert!(matches!(
        read.join().unwrap(),
        Ok(crate::services::provider::ReadOutputResult::Completed { .. })
    ));
    let frames: Vec<_> = rx.try_iter().collect();
    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, StreamMessage::Done { .. })),
        "{frames:?}"
    );
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame, StreamMessage::CodexTuiTerminalDone { .. })),
        "{frames:?}"
    );
    let path = inflight_state_path(
        &inflight_runtime_root().unwrap(),
        &case.provider,
        case.channel,
    );
    let row = String::from_utf8(std::fs::read(path).unwrap()).unwrap();
    assert!(!row.contains("tui_terminal_kind"), "{row}");
}
