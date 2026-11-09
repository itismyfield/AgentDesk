//! The public watchdog pass over a turn-mode channel whose turn crossed a restart: real native
//! reader, O answering through the real demand/watch read.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::services::discord::turn_presence::activity::ClosedBoundary;

type Hooks = HashMap<(&'static str, u64), Box<dyn FnOnce()>>;

thread_local! {
    static NATIVE: RefCell<HashMap<u64, Option<ClosedBoundary>>> = RefCell::default();
    static HOOKS: RefCell<Hooks> = RefCell::default();
    static EVENTS: RefCell<Vec<(u64, &'static str)>> = const { RefCell::new(Vec::new()) };
    static WITHHOLD: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

#[cfg(unix)]
pub(super) fn on_archive(channel: u64, hook: impl FnOnce() + 'static) {
    HOOKS.with(|hooks| {
        hooks
            .borrow_mut()
            .insert(("archive", channel), Box::new(hook))
    });
}

/// Whether this pass's finalize for `channel` is withheld once, as a finalizer that never ran.
pub(super) fn withhold_finalize(channel: u64) -> bool {
    WITHHOLD.with(|withhold| {
        let mut withhold = withhold.borrow_mut();
        let at = withhold.iter().position(|c| *c == channel);
        at.map(|at| withhold.remove(at)).is_some()
    })
}

/// A scripted native end, for a channel the activity reader cannot bind (no watcher).
pub(super) fn native_end(channel: u64) -> Option<Option<ClosedBoundary>> {
    NATIVE.with(|native| native.borrow().get(&channel).cloned())
}

pub(super) async fn pause(point: &'static str, channel: u64) {
    if let Some(hook) = HOOKS.with(|hooks| hooks.borrow_mut().remove(&(point, channel))) {
        hook();
    }
}

pub(super) fn note_event(channel: u64, event: &'static str) {
    EVENTS.with(|events| events.borrow_mut().push((channel, event)));
}

// The cases bind a real transcript through the turn-presence fixture, which only unix builds have.
#[cfg(unix)]
mod cases {
    #[path = "carried_ledger_tests.rs"]
    mod ledger;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use poise::serenity_prelude::{ChannelId, MessageId};

    use super::super::super::HealthRegistry;
    use super::*;
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::discord::InflightRestartMode;
    use crate::services::discord::inflight::{self, InflightTurnState};
    use crate::services::discord::{self, SharedData};
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::tui_o::writer::actor::{Owing, Undelivered};
    use crate::services::tui_o::writer::host::OwedView;

    fn events(channel: u64) -> Vec<&'static str> {
        EVENTS.with(|events| {
            let events = events.borrow();
            events
                .iter()
                .filter(|(c, _)| *c == channel)
                .map(|(_, e)| *e)
                .collect()
        })
    }

    fn on(point: &'static str, channel: u64, hook: impl FnOnce() + 'static) {
        HOOKS.with(|hooks| hooks.borrow_mut().insert((point, channel), Box::new(hook)));
    }

    const PROVIDER: ProviderKind = ProviderKind::Claude;

    fn user(id: &str) -> String {
        format!(r#"{{"type":"user","uuid":"{id}","message":{{"content":"carried input"}}}}"#)
    }
    const END: &str = r#"{"type":"system","subtype":"turn_duration"}"#;
    const INTERRUPT: &str =
        r#"{"type":"user","message":{"content":"[Request interrupted by user]"}}"#;
    const ENQUEUE: &str = r#"{"type":"queue-operation","operation":"enqueue","content":"next"}"#;
    const QUEUED_COMMAND: &str = r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"next","commandMode":"prompt","origin":{"kind":"human"},"humanTurn":true}}"#;
    const TASK_NOTIFICATION: &str = r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"<task-notification>done</task-notification>","commandMode":"task-notification","origin":{"kind":"task-notification"}}}"#;
    const DEQUEUE: &str = r#"{"type":"queue-operation","operation":"dequeue"}"#;

    fn enqueue(text: &str) -> String {
        format!(r#"{{"type":"queue-operation","operation":"enqueue","content":"{text}"}}"#)
    }

    fn prompt(text: &str) -> String {
        format!(r#"{{"type":"user","uuid":"u-{text}","message":{{"content":"{text}"}}}}"#)
    }

    /// A paste settled through the holder's real mailbox reservation, as the injection owner does.
    async fn settle_injection(
        fx: &Fixture,
        outcome: discord::inject_disposition::InjectionOutcome,
    ) {
        use crate::services::turn_orchestrator::{InjectionSettlement, ReserveOutcome};
        let snapshot = discord::mailbox_snapshot(&fx.shared, fx.channel).await;
        let claim = (snapshot.cancel_token.clone()).map(|token| {
            (
                token,
                snapshot.active_turn_kind,
                snapshot.active_user_message_id,
            )
        });
        let context = discord::queue_persistence_context(&fx.shared, &PROVIDER, fx.channel);
        let mailbox = fx.shared.mailbox(fx.channel);
        let ReserveOutcome::Reserved(ticket) = mailbox
            .reserve_injection(None, claim, context.clone())
            .await
        else {
            panic!("the carried holder takes the paste");
        };
        let settle = InjectionSettlement::Delivered(outcome);
        let _ = mailbox.settle_injected_input(ticket, settle, context).await;
    }
    /// One assistant reply row, named apart from every other so each is its own unit.
    fn answer() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        format!(
            r#"{{"type":"assistant","uuid":"u-answer-{id}","apiBlockIndex":0,"message":{{"id":"answer-{id}","content":[{{"type":"text","text":"answer"}}]}}}}"#
        )
    }

    fn append(path: &std::path::Path, rows: &[&str]) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for row in rows {
            writeln!(file, "{row}").unwrap();
        }
    }

    /// What the scripted O answers; empty `captured` reports the transcript's current length.
    #[derive(Clone, Default)]
    struct OAnswer {
        debt: Undelivered,
        behind: u64,
    }

    /// One channel's carried turn: an earlier generation's DrainRestart row restored as the mailbox
    /// episode, a confirmed turn-mode transcript, and O's owed read.
    struct Fixture {
        shared: Arc<SharedData>,
        pool: sqlx::PgPool,
        registry: HealthRegistry,
        channel: ChannelId,
        path: std::path::PathBuf,
        user_msg_id: u64,
        o: Arc<Mutex<OAnswer>>,
        _keep: Vec<Box<dyn std::any::Any>>,
    }

    struct Options {
        born_generation: u64,
        watcher: bool,
        turn_mode: bool,
        episode: bool,
        transcript: Vec<String>,
    }

    impl Default for Options {
        fn default() -> Self {
            Self {
                born_generation: 5,
                watcher: true,
                turn_mode: true,
                episode: true,
                transcript: vec![user("carried"), END.to_string()],
            }
        }
    }

    impl Fixture {
        async fn new(channel: u64, user_msg_id: u64, options: Options) -> Self {
            let mut keep: Vec<Box<dyn std::any::Any>> = Vec::new();
            let runtime = crate::config::TestRuntimeRootGuard::new();
            let root = tempfile::tempdir().unwrap();
            let binding = discord::health::BindingRoot::enter(root.path());
            let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let shared = discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
            let path = root.path().join(format!("{channel}.jsonl"));
            let rows: Vec<&str> = options.transcript.iter().map(String::as_str).collect();
            std::fs::write(&path, "").unwrap();
            append(&path, &rows);
            let session = format!("AgentDesk-claude-7533-{channel}");
            if options.watcher {
                keep.push(Box::new(discord::health::ReadyPane::mark(&session)));
                let confirmed = discord::health::bind_turn_mode_transcript(
                    &shared, &PROVIDER, channel, &session, &path,
                );
                if options.turn_mode {
                    keep.push(Box::new(confirmed));
                }
            } else if options.turn_mode {
                keep.push(Box::new(
                    crate::services::tui_o::turn_mode::TestConfirmation::new(channel),
                ));
            }
            keep.push(Box::new(
                crate::services::tui_o::cutover::test_override::force_channels(&[(
                    channel,
                    RuntimeHandoffKind::ClaudeTui,
                )]),
            ));

            let mut row = InflightTurnState::new(
                PROVIDER,
                channel,
                None,
                7,
                user_msg_id,
                user_msg_id + 1,
                "carried".to_string(),
                Some("session-7533".to_string()),
                Some(session.clone()),
                Some(path.display().to_string()),
                None,
                0,
            );
            row.born_generation = options.born_generation;
            row.turn_nonce = Some(format!("nonce-{channel}"));
            inflight::save_inflight_state(&row).unwrap();
            let mut row = inflight::load_inflight_state(&PROVIDER, channel).unwrap();
            if options.episode {
                discord::recovery_engine::reregister_active_turn_from_inflight(&shared, &row).await;
                row = inflight::load_inflight_state(&PROVIDER, channel).unwrap();
            }
            // The managed restart's mark, as the shutdown left it on the row the next process reads.
            row.restart_mode = Some(InflightRestartMode::DrainRestart);
            // The replacement window the restart handoff keeps: the mark names the generation the
            // test process reads as current, so no sweep of this pass may remove it as foreign.
            row.restart_generation = Some(shared.restart.current_generation);
            let file = inflight::inflight_state_path(
                &inflight::inflight_runtime_root().unwrap(),
                &PROVIDER,
                channel,
            );
            std::fs::write(&file, serde_json::to_vec(&row).unwrap()).unwrap();

            if options.watcher {
                // The reader's first sight of a binding rebuilds off-thread; production's next tick
                // reads what this waits for.
                discord::health::settled_reason(&shared, &PROVIDER, channel).await;
            }
            let registry = HealthRegistry::new();
            registry
                .register(PROVIDER.as_str().to_string(), shared.clone())
                .await;
            let o = Arc::new(Mutex::new(OAnswer::default()));
            host_o(channel, &path, o.clone());
            keep.push(Box::new(db));
            keep.push(Box::new(binding));
            keep.push(Box::new(root));
            keep.push(Box::new(runtime));
            Self {
                shared,
                pool,
                registry,
                channel: ChannelId::new(channel),
                path,
                user_msg_id,
                o,
                _keep: keep,
            }
        }

        async fn pass(&self) {
            super::super::super::run_stall_watchdog_pass(&self.registry, &PROVIDER).await;
        }

        fn row(&self) -> Option<InflightTurnState> {
            inflight::load_inflight_state(&PROVIDER, self.channel.get())
        }

        async fn episode_open(&self) -> bool {
            let snapshot = discord::mailbox_snapshot(&self.shared, self.channel).await;
            snapshot.cancel_token.is_some()
                && snapshot.active_user_message_id == Some(MessageId::new(self.user_msg_id))
        }

        fn archive_count(&self) -> usize {
            let dir = inflight::inflight_runtime_root().unwrap().join("archive");
            std::fs::read_dir(dir).map_or(0, |entries| {
                entries
                    .flatten()
                    .filter(|entry| {
                        entry.file_name().to_string_lossy().starts_with(&format!(
                            "{}.json.turn-mode-carried-turn",
                            self.channel.get()
                        ))
                    })
                    .count()
            })
        }

        fn archived(&self) -> bool {
            self.archive_count() != 0
        }

        async fn assert_kept(&self, case: &str) {
            assert!(self.row().is_some(), "{case}: the carried row stays");
            assert!(self.episode_open().await, "{case}: the episode stays open");
            assert!(
                events(self.channel.get()).is_empty(),
                "{case}: nothing was submitted"
            );
            assert!(!self.archived(), "{case}: nothing was archived");
        }

        /// Rewrites the row as an unproven idle managed input bound to the session the pane left.
        fn supersede_unproven(&self) -> std::path::PathBuf {
            let mut row = self.row().unwrap();
            row.managed_submission = Some(
                serde_json::from_value(serde_json::json!({ "protocol": 1, "phase": "waiting" }))
                    .unwrap(),
            );
            row.readopted_from_inflight = false;
            row.native_stop_user_pin = None;
            row.session_id = Some("session-before-clear".to_string());
            let file = inflight::inflight_state_path(
                &inflight::inflight_runtime_root().unwrap(),
                &PROVIDER,
                self.channel.get(),
            );
            std::fs::write(&file, serde_json::to_vec(&row).unwrap()).unwrap();
            file
        }

        /// The unproven-close notices queued for this channel.
        async fn told(&self) -> i64 {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM message_outbox
                 WHERE reason_code = 'carried.unverified_closed' AND target = $1",
            )
            .bind(format!("channel:{}", self.channel.get()))
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }

        async fn assert_closed(&self, case: &str) {
            assert!(self.row().is_none(), "{case}: the carried row is gone");
            assert!(self.archived(), "{case}: the row was archived, not dropped");
            assert!(
                !self.episode_open().await,
                "{case}: the episode is released"
            );
        }
    }

    /// Answers O's owed read through the real demand/watch path for `channel`.
    fn host_o(channel: u64, path: &std::path::Path, answer: Arc<Mutex<OAnswer>>) {
        let owing = Owing::default();
        let view = OwedView {
            published: owing.published.subscribe(),
            demand: owing.demand.clone(),
        };
        crate::services::tui_o::writer::host::process_readiness()
            .track_owed_for_test(channel, view);
        let path = path.to_path_buf();
        tokio::spawn(async move {
            // Ends with the test's runtime.
            loop {
                if owing.demand.wanted() {
                    let script = answer.lock().unwrap().clone();
                    let len = std::fs::metadata(&path).map_or(0, |m| m.len());
                    let mut owed = script.debt;
                    owed.asked = owing.demand.asked();
                    if owed.captured.is_empty() {
                        owed.captured = vec![(path.clone(), len.saturating_sub(script.behind))];
                    }
                    owing.published.send_replace(Some(owed));
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
    }

    /// The fixture's restored row still carries the restart mark the archive must get past.
    #[tokio::test(flavor = "current_thread")]
    async fn carried_close_fixture_preserves_drain_restart_at_entry() {
        let fx = Fixture::new(7_533_301, 9_100_095_491_915_387_881, Options::default()).await;
        let row = fx.row().unwrap();
        assert_eq!(row.restart_mode, Some(InflightRestartMode::DrainRestart));
        assert!(fx.episode_open().await, "the restore re-minted the episode");
    }

    /// The managed-restart shape: a DrainRestart row of the earlier generation, its re-minted
    /// episode, the native end read and O drained through it — archived, released, completed.
    #[tokio::test(flavor = "current_thread")]
    async fn carried_drain_restart_row_is_archived_and_episode_released() {
        // A headless (synthetic id) and a Discord (snowflake) turn close by the same rule.
        for (channel, user_msg_id) in [
            (7_533_311, 9_100_095_491_915_387_881),
            (7_533_312, 1_479_671_298_497_183_900),
        ] {
            let fx = Fixture::new(channel, user_msg_id, Options::default()).await;
            assert_eq!(
                fx.row().unwrap().restart_mode,
                Some(InflightRestartMode::DrainRestart)
            );
            fx.pass().await;
            fx.assert_closed("crossed turn").await;
            assert_eq!(events(channel), ["complete"]);
            fx.pass().await;
            assert_eq!(
                events(channel),
                ["complete"],
                "a closed turn is not closed again"
            );
        }
    }

    /// An input received for the holder after its last close, whose run has not
    /// opened, keeps the carried episode — both receipt shapes Claude writes.
    #[tokio::test(flavor = "current_thread")]
    async fn observed_enqueue_before_native_open_keeps_carried_episode() {
        for (channel, receipt) in [(7_533_321, ENQUEUE), (7_533_322, QUEUED_COMMAND)] {
            let fx = Fixture::new(channel, 9_100_095_491_915_388_001, Options::default()).await;
            append(&fx.path, &[receipt]);
            fx.pass().await;
            fx.assert_kept(receipt).await;

            // The run opens: the episode stays the stall verdict's open turn, aged past the page
            // threshold, exactly as the verdict reads it (`TurnFacts::new`).
            append(&fx.path, &[&prompt("next"), &answer()]);
            fx.pass().await;
            fx.assert_kept("its run is open").await;
            let mailbox = fx.shared.mailbox_peek(fx.channel).unwrap();
            mailbox
                .age_active_turn_for_test(Duration::from_secs(700))
                .await;
            fx.pass().await;
            fx.assert_kept("its run is still open").await;
            let snapshot = discord::mailbox_snapshot(&fx.shared, fx.channel).await;
            let turn_open =
                snapshot.cancel_token.is_some() || snapshot.active_user_message_id.is_some();
            let age = super::super::super::super::transcript_binding_stall::turn_age_secs(
                &snapshot, None,
            );
            assert!(turn_open, "the verdict still sees the turn open");
            assert!(
                age.is_some_and(|age| age >= 600),
                "and past the 600 s page threshold: {age:?}"
            );

            // Its own end and O's delivery close it.
            append(&fx.path, &[END]);
            fx.pass().await;
            fx.assert_closed("its run ended and O delivered it").await;
        }
    }

    /// The deploy shape: an earlier generation's managed row re-saved after restart, above history
    /// whose queued background notifications were drained mid-turn and so owe nothing.
    #[tokio::test(flavor = "current_thread")]
    async fn drained_notifications_in_history_do_not_hold_a_carried_managed_row() {
        let note = "<task-notification>job done</task-notification>";
        let notification = format!(
            r#"{{"type":"attachment","attachment":{{"type":"queued_command","prompt":"{note}","commandMode":"task-notification","origin":{{"kind":"task-notification"}}}}}}"#
        );
        let options = Options {
            transcript: vec![
                user("earlier"),
                enqueue(note),
                notification,
                answer(),
                END.to_string(),
                user("carried"),
                answer(),
                END.to_string(),
            ],
            ..Options::default()
        };
        let fx = Fixture::new(7_533_336, 9_100_095_491_915_387_881, options).await;
        // This process saved the row again after the restart.
        let mut row = fx.row().unwrap();
        assert_eq!(row.turn_source, inflight::TurnSource::Managed);
        row.save_generation += 1;
        let file = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &PROVIDER,
            fx.channel.get(),
        );
        std::fs::write(&file, serde_json::to_vec(&row).unwrap()).unwrap();
        fx.pass().await;
        fx.assert_closed("the drained notifications owed nothing")
            .await;
        assert_eq!(events(fx.channel.get()), ["complete"]);
    }

    /// History with an input Claude absorbed mid-turn and wrote no command for, then an input it
    /// dequeued under an opener naming another body: both ran and closed, so the carried row closes.
    #[tokio::test(flavor = "current_thread")]
    async fn absorbed_inputs_without_a_command_do_not_hold_a_carried_row() {
        let absorbed = r#"{"type":"queue-operation","operation":"remove","reason":"absorbed_mid_turn","content":"human b"}"#;
        let options = Options {
            transcript: vec![
                user("earlier"),
                enqueue("human b"),
                absorbed.to_string(),
                answer(),
                END.to_string(),
                enqueue("c"),
                DEQUEUE.to_string(),
                prompt("another session sent c"),
                answer(),
                END.to_string(),
                user("carried"),
                answer(),
                END.to_string(),
            ],
            ..Options::default()
        };
        let fx = Fixture::new(7_533_399, 9_100_095_491_915_388_099, options).await;
        fx.pass().await;
        fx.assert_closed("the absorbed and the dequeued input ran and closed")
            .await;
        assert_eq!(events(fx.channel.get()), ["complete"]);
    }

    /// A kept carried turn names its reason once per change, never each pass.
    #[tokio::test(flavor = "current_thread")]
    async fn a_kept_turn_logs_its_reason_once_per_change() {
        let fx = Fixture::new(7_533_337, 9_100_095_491_915_388_016, Options::default()).await;
        append(&fx.path, &[&enqueue("waiting")]);
        let logs = Logs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(logs.clone())
            .finish();
        crate::logging::test_capture::pin_callsite_interest();
        let guard = tracing::subscriber::set_default(subscriber);
        fx.pass().await;
        fx.pass().await;
        drop(guard);
        let logs = logs.text();
        let kept = "no_native_end:pending_receipt:owed_inputs=1";
        assert_eq!(logs.matches(kept).count(), 1, "{logs}");
        fx.assert_kept("waiting").await;
    }

    /// The hold diagnostics each pass logs for the channel, with `prepare(pass)` run before it.
    async fn held_lines(fx: &Fixture, passes: usize, prepare: impl Fn(usize)) -> Vec<Vec<String>> {
        let channel = format!("channel_id={}", fx.channel.get());
        let logs = Logs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(logs.clone())
            .finish();
        crate::logging::test_capture::pin_callsite_interest();
        let guard = tracing::subscriber::set_default(subscriber);
        let mut per_pass = Vec::new();
        for pass in 0..passes {
            prepare(pass);
            let before = logs.text().len();
            fx.pass().await;
            let text = logs.text();
            let held = text[before..]
                .lines()
                .filter(|line| line.contains(&channel) && line.contains("carried-over turn"))
                .filter(|line| !line.contains("turn closed"))
                .map(str::to_owned)
                .collect();
            per_pass.push(held);
        }
        drop(guard);
        per_pass
    }

    /// Rewrites the channel's inflight row with the next save generation, as a concurrent save.
    fn bump_save_generation(channel: u64) {
        let file = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &PROVIDER,
            channel,
        );
        let mut row: InflightTurnState =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        row.save_generation += 1;
        std::fs::write(&file, serde_json::to_vec(&row).unwrap()).unwrap();
    }

    /// An archived turn whose episode stays open warns once per change: a first reason is named,
    /// the same reason next pass is not, a changed reason is named again.
    #[tokio::test(flavor = "current_thread")]
    async fn an_open_episode_after_the_archive_is_logged_once_per_change() {
        let fx = Fixture::new(7_533_397, 9_100_095_491_915_388_077, Options::default()).await;
        let channel = fx.channel.get();
        WITHHOLD.with(|withhold| withhold.borrow_mut().extend([channel, channel]));
        let held = held_lines(&fx, 3, |pass| {
            // The third pass finds an input queued after the end: another reason to keep it.
            if pass == 2 {
                append(&fx.path, &[&enqueue("later")]);
            }
        })
        .await;
        assert_eq!(
            held.iter().map(Vec::len).collect::<Vec<_>>(),
            [1, 0, 1],
            "{held:?}"
        );
        assert!(held[0][0].trim_start().starts_with("WARN"), "{held:?}");
    }

    /// A row that changes before each archive names that hold once; the open episode a later
    /// archive leaves is a changed reason, named again.
    #[tokio::test(flavor = "current_thread")]
    async fn a_repeated_archive_failure_is_logged_once_per_change() {
        let fx = Fixture::new(7_533_398, 9_100_095_491_915_388_078, Options::default()).await;
        let channel = fx.channel.get();
        let held = held_lines(&fx, 3, |pass| match pass {
            2 => WITHHOLD.with(|withhold| withhold.borrow_mut().push(channel)),
            _ => on("archive", channel, move || bump_save_generation(channel)),
        })
        .await;
        assert_eq!(
            held.iter().map(Vec::len).collect::<Vec<_>>(),
            [1, 0, 1],
            "{held:?}"
        );
    }

    #[derive(Clone, Default)]
    struct Logs(Arc<Mutex<Vec<u8>>>);

    impl Logs {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
        type Writer = Logs;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// An unconfirmed paste settled after the transcript last grew may have reached the pane unrecorded.
    #[tokio::test(flavor = "current_thread")]
    async fn injection_delivered_after_last_growth_keeps_carried_episode() {
        use discord::inject_disposition::InjectionOutcome;
        let fx = Fixture::new(7_533_332, 9_100_095_491_915_388_012, Options::default()).await;
        // The reader has read the close; the paste settles after it with no receipt.
        let _ =
            discord::turn_presence::activity::reading_now(&fx.shared, &PROVIDER, fx.channel).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        settle_injection(&fx, InjectionOutcome::Unconfirmed).await;
        fx.pass().await;
        fx.assert_kept("unconfirmed after the last growth").await;
        append(&fx.path, &[&user("injected"), &answer(), END]);
        fx.pass().await;
        fx.assert_closed("the injected run ended").await;
    }

    /// An observed paste's input already ran and closed when its settlement lands late: the closed
    /// input is not held for the next transcript growth.
    #[tokio::test(flavor = "current_thread")]
    async fn late_observed_settlement_does_not_hold_closed_input() {
        use discord::inject_disposition::InjectionOutcome;
        let fx = Fixture::new(7_533_333, 9_100_095_491_915_388_013, Options::default()).await;
        append(
            &fx.path,
            &[
                &enqueue("pasted"),
                DEQUEUE,
                &prompt("pasted"),
                &answer(),
                END,
            ],
        );
        let _ =
            discord::turn_presence::activity::reading_now(&fx.shared, &PROVIDER, fx.channel).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        settle_injection(&fx, InjectionOutcome::Observed).await;
        fx.pass().await;
        fx.assert_closed("its run closed before the settlement")
            .await;
        assert_eq!(events(fx.channel.get()), ["complete"]);
    }

    /// A background task's notification command consumes no human input: B, received while A ran,
    /// keeps the carried episode open after A closes, until B's own run closes.
    #[tokio::test(flavor = "current_thread")]
    async fn unrelated_task_notification_cannot_consume_pending_human_input() {
        use discord::inject_disposition::InjectionOutcome;
        let options = Options {
            transcript: vec![user("a")],
            ..Options::default()
        };
        let fx = Fixture::new(7_533_334, 9_100_095_491_915_388_014, options).await;
        append(&fx.path, &[&enqueue("human b")]);
        settle_injection(&fx, InjectionOutcome::Observed).await;
        append(&fx.path, &[TASK_NOTIFICATION, &answer(), END]);
        fx.pass().await;
        fx.assert_kept("B received, not run").await;
        append(&fx.path, &[DEQUEUE, &prompt("human b"), &answer()]);
        fx.pass().await;
        fx.assert_kept("B running").await;
        append(&fx.path, &[END]);
        fx.pass().await;
        fx.assert_closed("B ran and closed").await;
    }

    /// Of two queued inputs, the one consumed and closed does not settle the other.
    #[tokio::test(flavor = "current_thread")]
    async fn one_consumed_receipt_does_not_clear_other_queued_input() {
        let options = Options {
            transcript: vec![user("a"), enqueue("b"), enqueue("c"), END.to_string()],
            ..Options::default()
        };
        let fx = Fixture::new(7_533_335, 9_100_095_491_915_388_015, options).await;
        fx.pass().await;
        fx.assert_kept("b and c queued").await;
        append(&fx.path, &[DEQUEUE, &prompt("b"), &answer(), END]);
        fx.pass().await;
        fx.assert_kept("c still queued").await;
        append(&fx.path, &[DEQUEUE, &prompt("c"), &answer(), END]);
        fx.pass().await;
        fx.assert_closed("both ran").await;
    }

    /// Injection holds the session transition lock through its paste and settlement.
    #[tokio::test(flavor = "current_thread")]
    async fn injection_holding_lock_blocks_close() {
        let fx = Fixture::new(7_533_341, 9_100_095_491_915_388_021, Options::default()).await;
        let held = fx
            .shared
            .session_transition_lock(fx.channel)
            .try_lock_owned()
            .unwrap();
        fx.pass().await;
        fx.assert_kept("lock held").await;
        drop(held);
        fx.pass().await;
        fx.assert_closed("lock free").await;
    }

    /// The transcript grows between O's read and the locked re-read: nothing closes this tick.
    #[tokio::test(flavor = "current_thread")]
    async fn native_growth_after_owed_read_blocks_close() {
        let fx = Fixture::new(7_533_342, 9_100_095_491_915_388_022, Options::default()).await;
        let path = fx.path.clone();
        on("locked", fx.channel.get(), move || {
            append(&path, &[&user("late")])
        });
        fx.pass().await;
        fx.assert_kept("grew under the lock").await;
        let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE turn_id=$1")
            .bind(format!("discord:{}:{}", fx.channel.get(), fx.user_msg_id))
            .fetch_one(&fx.pool)
            .await
            .unwrap();
        assert_eq!(
            receipts, 0,
            "stale native evidence must not commit a receipt"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_growth_after_receipt_commit_keeps_the_row_and_episode_pg() {
        let fx = Fixture::new(7_807_343, 9_100_095_491_915_388_023, Options::default()).await;
        let path = fx.path.clone();
        on("receipt", fx.channel.get(), move || {
            append(&path, &[&user("after-receipt")])
        });
        fx.pass().await;
        fx.assert_kept("native turn opened after receipt commit")
            .await;
        assert_eq!(fx.archive_count(), 0);
        let outcome: String =
            sqlx::query_scalar("SELECT terminal_outcome FROM turns WHERE turn_id=$1")
                .bind(format!("discord:{}:{}", fx.channel.get(), fx.user_msg_id))
                .fetch_one(&fx.pool)
                .await
                .unwrap();
        assert_eq!(
            outcome, "completed",
            "a committed receipt alone cannot archive"
        );
    }

    /// A row rewritten just before the archive keeps both the row and the episode.
    #[tokio::test(flavor = "current_thread")]
    async fn archive_pin_drift_blocks_finalize() {
        let fx = Fixture::new(7_533_351, 9_100_095_491_915_388_031, Options::default()).await;
        let channel = fx.channel.get();
        on("archive", channel, move || bump_save_generation(channel));
        fx.pass().await;
        fx.assert_kept("pin drifted").await;
    }

    /// O still owing anything, or behind the native end, keeps the turn.
    #[tokio::test(flavor = "current_thread")]
    async fn o_debt_or_capture_behind_end_keeps_carried_turn() {
        let cases: [(u64, OAnswer); 6] = [
            (
                7_533_361,
                OAnswer {
                    behind: 1,
                    ..OAnswer::default()
                },
            ),
            (
                7_533_362,
                OAnswer {
                    debt: Undelivered {
                        owed: 20,
                        notice_owed: 20,
                        posting: 1,
                        ..Undelivered::default()
                    },
                    behind: 0,
                },
            ),
            (
                7_533_363,
                OAnswer {
                    debt: Undelivered {
                        binding_pending: 1,
                        ..Undelivered::default()
                    },
                    behind: 0,
                },
            ),
            (
                7_533_364,
                OAnswer {
                    debt: Undelivered {
                        verified_work: 1,
                        ..Undelivered::default()
                    },
                    behind: 0,
                },
            ),
            (
                7_807_365,
                OAnswer {
                    debt: Undelivered {
                        owed: 21,
                        notice_owed: 20,
                        ..Undelivered::default()
                    },
                    behind: 0,
                },
            ),
            (
                7_807_366,
                OAnswer {
                    debt: Undelivered {
                        owed: 20,
                        notice_owed: 20,
                        prepared: 1,
                        ..Undelivered::default()
                    },
                    behind: 0,
                },
            ),
        ];
        for (channel, answer) in cases {
            let fx = Fixture::new(channel, 9_100_095_491_915_388_041, Options::default()).await;
            *fx.o.lock().unwrap() = answer;
            fx.pass().await;
            fx.assert_kept("O not drained").await;
            *fx.o.lock().unwrap() = OAnswer::default();
            fx.pass().await;
            fx.assert_closed("O drained").await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unadopted_managed_carried_row_without_native_pin_is_kept_pg() {
        let fx = Fixture::new(7_807_371, 9_100_095_491_915_388_050, Options::default()).await;
        let mut row = fx.row().unwrap();
        row.managed_submission = Some(
            serde_json::from_value(serde_json::json!({ "protocol": 1, "phase": "waiting" }))
                .unwrap(),
        );
        row.readopted_from_inflight = false;
        row.native_stop_user_pin = None;
        // The row names the session the fixture's binding still runs ("parent").
        row.session_id = Some("parent".to_string());
        let path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &PROVIDER,
            fx.channel.get(),
        );
        std::fs::write(path, serde_json::to_vec(&row).unwrap()).unwrap();
        fx.pass().await;
        fx.assert_kept("unadopted managed input has no verified native pin")
            .await;
    }

    /// An unproven managed row bound to the session the pane left closes once the pane's own
    /// turn ended and O delivered it; its channel is told once, even when a pass is repeated.
    #[tokio::test(flavor = "current_thread")]
    async fn unproven_input_of_a_superseded_session_closes_with_one_notice_pg() {
        let fx = Fixture::new(7_896_201, 9_100_096_243_727_098_100, Options::default()).await;
        let file = fx.supersede_unproven();
        let told = || fx.told();
        // The first pass's archive loses to a re-save, as a restart's rewrite would.
        let resaved = file.clone();
        on("archive", fx.channel.get(), move || {
            let mut row: InflightTurnState =
                serde_json::from_slice(&std::fs::read(&resaved).unwrap()).unwrap();
            row.save_generation += 1;
            std::fs::write(&resaved, serde_json::to_vec(&row).unwrap()).unwrap();
        });
        fx.pass().await;
        assert!(fx.row().is_some(), "the lost archive keeps the row");
        assert_eq!(
            told().await,
            1,
            "the close told its channel before archiving"
        );
        fx.pass().await;
        fx.assert_closed("the superseded session's input").await;
        assert_eq!(fx.archive_count(), 1);
        fx.pass().await;
        assert_eq!(told().await, 1, "a repeated close tells the channel once");
    }

    /// A receipt that fails to commit on an unproven close leaves no settlement mark, so the next
    /// admission cannot remove the row without its notice; the later close tells the channel once.
    #[tokio::test(flavor = "current_thread")]
    async fn an_unproven_close_whose_receipt_fails_is_never_settled_without_its_notice_pg() {
        let fx = Fixture::new(7_896_204, 9_100_096_243_727_098_104, Options::default()).await;
        fx.supersede_unproven();
        let turns = |sql: &'static str| sqlx::query(sql).execute(&fx.pool);
        turns("ALTER TABLE turns RENAME TO turns_unavailable")
            .await
            .unwrap();
        fx.pass().await;
        let kept = fx.row().expect("the failed receipt keeps the row");
        assert_eq!(kept.terminal_receipt, None, "no settlement mark");
        assert_eq!(fx.told().await, 0);
        turns("ALTER TABLE turns_unavailable RENAME TO turns")
            .await
            .unwrap();
        let settled =
            discord::receipt_settle::settle_marked_row(&fx.shared, &PROVIDER, fx.channel.get())
                .await;
        assert!(!settled && fx.row().is_some(), "admission leaves the row");
        fx.pass().await;
        fx.assert_closed("the recovered receipt").await;
        assert_eq!(fx.told().await, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn input_notice_debt_alone_closes_and_archives_the_carried_row_once_pg() {
        let fx = Fixture::new(7_807_367, 9_100_095_491_915_388_047, Options::default()).await;
        fx.o.lock().unwrap().debt = Undelivered {
            owed: 20,
            notice_owed: 20,
            ..Undelivered::default()
        };
        fx.pass().await;
        fx.assert_closed("only input notices remain").await;
        assert_eq!(fx.archive_count(), 1);
        assert_eq!(events(fx.channel.get()), ["complete"]);
        fx.pass().await;
        assert_eq!(fx.archive_count(), 1, "A is archived exactly once");
        assert_eq!(events(fx.channel.get()), ["complete"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn receipt_commit_failure_keeps_a_carried_row_with_or_without_origins_pg() {
        for (channel, origins) in [(7_807_368, false), (7_807_369, true)] {
            let fx = Fixture::new(channel, 9_100_095_491_915_388_048, Options::default()).await;
            if origins {
                let mut row = fx.row().unwrap();
                row.input_origin_ids = vec![format!("carried-origin-{channel}")];
                inflight::save_inflight_state(&row).unwrap();
            }
            fx.pool.close().await;
            fx.pass().await;
            fx.assert_kept(&format!("PG commit unavailable, origins={origins}"))
                .await;
            assert_eq!(fx.archive_count(), 0);
            let stamp = fx
                .row()
                .unwrap()
                .terminal_receipt
                .expect("completion mark retained");
            assert_eq!(stamp.outcome, crate::db::turns::TerminalOutcome::Completed);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn completed_receipt_is_visible_on_an_independent_connection_before_archive_pg() {
        let fx = Fixture::new(7_807_370, 9_100_095_491_915_388_049, Options::default()).await;
        let options = fx.pool.connect_options().as_ref().clone();
        let turn = format!("discord:{}:{}", fx.channel.get(), fx.user_msg_id);
        let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = observed.clone();
        on("archive", fx.channel.get(), move || {
            // The hook blocks its caller; a fresh runtime and connection keep its PG read live.
            let outcome = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async move {
                    let pool = sqlx::postgres::PgPoolOptions::new()
                        .max_connections(1)
                        .acquire_timeout(Duration::from_secs(5))
                        .connect_with(options)
                        .await
                        .unwrap();
                    let outcome: Option<String> =
                        sqlx::query_scalar("SELECT terminal_outcome FROM turns WHERE turn_id=$1")
                            .bind(turn)
                            .fetch_optional(&pool)
                            .await
                            .unwrap();
                    pool.close().await;
                    outcome
                })
            })
            .join()
            .unwrap();
            assert_eq!(
                outcome.as_deref(),
                Some("completed"),
                "PG commit precedes archive"
            );
            seen.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        fx.pass().await;
        assert!(observed.load(std::sync::atomic::Ordering::SeqCst));
        fx.assert_closed("independently committed before archive")
            .await;
        assert_eq!(fx.archive_count(), 1);
    }

    /// An interrupted native end settles the episode as cancelled, never completed.
    #[tokio::test(flavor = "current_thread")]
    async fn interrupted_close_cancels() {
        let options = Options {
            transcript: vec![user("carried"), INTERRUPT.to_string()],
            ..Options::default()
        };
        let fx = Fixture::new(7_533_371, 9_100_095_491_915_388_051, options).await;
        fx.pass().await;
        fx.assert_closed("interrupted").await;
        assert_eq!(events(fx.channel.get()), ["cancel"]);
        let receipt: String =
            sqlx::query_scalar("SELECT terminal_outcome FROM turns WHERE turn_id=$1")
                .bind(format!("discord:{}:{}", fx.channel, fx.user_msg_id))
                .fetch_one(&fx.pool)
                .await
                .unwrap();
        assert_eq!(receipt, "cancelled");
    }

    /// A managed input with its own mailbox episode keeps that episode's cancellation outcome.
    #[tokio::test(flavor = "current_thread")]
    async fn interrupted_unreadopted_managed_input_with_pin_cancels_its_episode_pg() {
        use crate::services::tui_input::human_frame::history;
        use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding};
        use crate::test_support::wait::{WAIT_CAP, await_within};
        use std::os::unix::fs::MetadataExt;
        use std::sync::atomic::AtomicBool;

        let attempt = "managed-interrupted-attempt";
        let frame = crate::services::claude_tui::busy_inject::frame(
            "discord", "7", "ann", attempt, "carried",
        );
        let input = serde_json::json!({"type":"user","uuid":"managed-interrupted-input",
            "sessionId":"session-7533","message":{"role":"user","content":frame}});
        let options = Options {
            transcript: vec![input.to_string(), INTERRUPT.to_string()],
            ..Options::default()
        };
        let fx = await_within(
            Fixture::new(7_828_371, 9_100_095_491_915_388_052, options),
            WAIT_CAP,
        )
        .await;
        let mut row = fx.row().unwrap();
        row.managed_submission = Some(
            serde_json::from_value(serde_json::json!({"protocol":1,"phase":"waiting"})).unwrap(),
        );
        row.readopted_from_inflight = false;
        row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        let row_path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &PROVIDER,
            fx.channel.get(),
        );
        std::fs::write(row_path, serde_json::to_vec(&row).unwrap()).unwrap();
        let token = await_within(discord::mailbox_snapshot(&fx.shared, fx.channel), WAIT_CAP)
            .await
            .cancel_token
            .unwrap();
        assert_eq!(token.turn_nonce(), row.turn_nonce.as_deref());
        let session = row.tmux_session_name.as_deref().unwrap();
        let marker = crate::services::tmux_common::session_temp_path(session, "spawn_nonce");
        std::fs::write(marker, "managed-interrupted-incarnation").unwrap();
        tui_prompt_dedupe::register_tmux_runtime_binding(
            session,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: fx.path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: row.session_id.clone(),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        history::register(("claude", session, attempt, "7"), &fx.path, 0, &frame).unwrap();
        inflight::bind_native_stop_pin(&row, &token);
        let metadata = fx.path.metadata().unwrap();
        let source = serde_json::json!({"kind":"existing","session_id":row.session_id,
            "transcript":fx.path,"file":[metadata.dev(),metadata.ino()],"offset":0});
        inflight::BusyNativeInput::capture(
            session,
            attempt,
            &frame,
            &fx.path,
            &source,
            Arc::new(AtomicBool::new(true)),
            token,
        )
        .unwrap()
        .adopt(0);
        tui_prompt_dedupe::clear_tmux_runtime_binding(session);
        let pinned = fx.row().unwrap();
        assert!(pinned.managed_submission.is_some());
        assert!(!pinned.readopted_from_inflight);
        assert!(pinned.terminal_receipt.is_none());
        assert!(inflight::historical_input_verified(&pinned));
        assert!(await_within(fx.episode_open(), WAIT_CAP).await);

        await_within(fx.pass(), WAIT_CAP).await;
        await_within(fx.assert_closed("managed interrupted episode"), WAIT_CAP).await;
        let receipt: String = await_within(
            sqlx::query_scalar("SELECT terminal_outcome FROM turns WHERE turn_id=$1")
                .bind(format!("discord:{}:{}", fx.channel, fx.user_msg_id))
                .fetch_one(&fx.pool),
            WAIT_CAP,
        )
        .await
        .unwrap();
        assert_eq!(receipt, "cancelled");
        assert_eq!(events(fx.channel.get()), ["cancel"]);
        assert_eq!(fx.archive_count(), 1);
        await_within(fx.pass(), WAIT_CAP).await;
        assert_eq!(events(fx.channel.get()), ["cancel"]);
        assert_eq!(fx.archive_count(), 1);
    }

    /// A pane with no turn evidence reads Idle, but not `closed`: nothing ended, nothing closes.
    #[tokio::test(flavor = "current_thread")]
    async fn no_turn_evidence_ready_is_not_closed() {
        let options = Options {
            transcript: Vec::new(),
            ..Options::default()
        };
        let fx = Fixture::new(7_533_372, 9_100_095_491_915_388_052, options).await;
        fx.pass().await;
        fx.assert_kept("no turn evidence").await;
    }

    /// This process's own open episode is the live bridge's to finish, and stays the verdict's.
    #[tokio::test(flavor = "current_thread")]
    async fn same_generation_open_episode_is_untouched() {
        let options = Options {
            born_generation: 0,
            ..Options::default()
        };
        let fx = Fixture::new(7_533_381, 9_100_095_491_915_388_061, options).await;
        fx.pass().await;
        fx.assert_kept("same generation").await;
    }

    /// A channel not confirmed for turn mode is left as main leaves it.
    #[tokio::test(flavor = "current_thread")]
    async fn not_turn_mode_is_unchanged() {
        let options = Options {
            turn_mode: false,
            ..Options::default()
        };
        let fx = Fixture::new(7_533_382, 9_100_095_491_915_388_062, options).await;
        fx.pass().await;
        fx.assert_kept("not turn mode").await;
    }

    /// A successor under the same message id but another nonce is not the carried episode.
    #[tokio::test(flavor = "current_thread")]
    async fn other_nonce_with_same_message_id_is_untouched() {
        let options = Options {
            episode: false,
            ..Options::default()
        };
        let fx = Fixture::new(7_533_383, 9_100_095_491_915_388_063, options).await;
        let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
            "successor".into(),
        )));
        assert!(
            discord::mailbox_try_start_turn(
                &fx.shared,
                fx.channel,
                token,
                poise::serenity_prelude::UserId::new(7),
                MessageId::new(fx.user_msg_id),
            )
            .await
        );
        fx.pass().await;
        assert!(fx.row().is_some(), "the row stays");
        assert!(fx.episode_open().await, "the successor stays");
        assert!(events(fx.channel.get()).is_empty());
    }

    /// A row whose episode is already released is archived without any finalize.
    #[tokio::test(flavor = "current_thread")]
    async fn episode_released_row_left_is_archived() {
        let options = Options {
            episode: false,
            ..Options::default()
        };
        let fx = Fixture::new(7_533_391, 9_100_095_491_915_388_071, options).await;
        fx.pass().await;
        assert!(fx.row().is_none());
        assert!(fx.archived());
        assert!(
            events(fx.channel.get()).is_empty(),
            "no episode, no terminal"
        );
    }

    /// A confirmed channel with no watcher is still a candidate.
    #[tokio::test(flavor = "current_thread")]
    async fn watcherless_confirmed_channel_is_closed() {
        let options = Options {
            watcher: false,
            ..Options::default()
        };
        let fx = Fixture::new(7_533_392, 9_100_095_491_915_388_072, options).await;
        let len = std::fs::metadata(&fx.path).unwrap().len();
        NATIVE.with(|native| {
            native.borrow_mut().insert(
                fx.channel.get(),
                Some(ClosedBoundary {
                    source: fx.path.clone(),
                    session: String::new(),
                    through: len,
                    grew_at: std::time::Instant::now() - Duration::from_secs(60),
                    aborted: false,
                }),
            )
        });
        fx.pass().await;
        fx.assert_closed("watcherless").await;
    }

    /// The first finalize never runs: the archived row's episode is released from memory on the next
    /// pass, once, and a successor under another nonce is never released by that memory.
    #[tokio::test(flavor = "current_thread")]
    async fn archived_target_retry_is_idempotent() {
        let fx = Fixture::new(7_533_393, 9_100_095_491_915_388_073, Options::default()).await;
        let channel = fx.channel.get();
        WITHHOLD.with(|withhold| withhold.borrow_mut().push(channel));
        fx.pass().await;
        assert!(fx.row().is_none() && fx.archived(), "archived first");
        assert!(
            fx.episode_open().await,
            "the withheld finalize left the episode"
        );
        assert!(events(channel).is_empty());
        assert!(
            super::super::archived().contains_key(&channel),
            "remembered for retry"
        );
        fx.pass().await;
        assert!(!fx.episode_open().await, "released by the retry");
        assert_eq!(events(channel), ["complete"]);
        assert!(!super::super::archived().contains_key(&channel));
        fx.pass().await;
        assert_eq!(events(channel), ["complete"], "retry is not repeated");
    }

    /// A successor that took the slot before the retry is not the remembered episode.
    #[tokio::test(flavor = "current_thread")]
    async fn archived_target_retry_spares_a_successor() {
        let fx = Fixture::new(7_533_395, 9_100_095_491_915_388_075, Options::default()).await;
        let channel = fx.channel.get();
        WITHHOLD.with(|withhold| withhold.borrow_mut().push(channel));
        fx.pass().await;
        discord::mailbox_finish_turn(&fx.shared, &PROVIDER, fx.channel).await;
        let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
            "successor".into(),
        )));
        let owner = poise::serenity_prelude::UserId::new(7);
        let message = MessageId::new(fx.user_msg_id);
        assert!(
            discord::mailbox_try_start_turn(&fx.shared, fx.channel, token, owner, message).await
        );
        fx.pass().await;
        let snapshot = discord::mailbox_snapshot(&fx.shared, fx.channel).await;
        assert_eq!(
            snapshot.active_turn_nonce.as_deref(),
            Some("successor"),
            "successor kept"
        );
        assert!(
            events(channel).is_empty(),
            "nothing submitted against the successor"
        );
        assert!(
            !super::super::archived().contains_key(&channel),
            "the stale memory is dropped"
        );
    }

    /// Once closed, the restored episode no longer holds the channel against queued input.
    #[tokio::test(flavor = "current_thread")]
    async fn queue_is_unblocked_after_close() {
        let fx = Fixture::new(7_533_394, 9_100_095_491_915_388_074, Options::default()).await;
        assert!(discord::mailbox_has_blocking_active_turn(&fx.shared, fx.channel).await);
        fx.pass().await;
        fx.assert_closed("closed").await;
        assert!(
            !discord::mailbox_has_blocking_active_turn(&fx.shared, fx.channel).await,
            "a queued input's dequeue is no longer held by the carried episode"
        );
    }

    /// A close wakes the queue a refused front waits in exactly once: through the finalizer's
    /// mailbox release when it ended a restored episode, by its own kick when none was restored.
    #[tokio::test(flavor = "current_thread")]
    async fn a_close_kicks_the_waiting_queue_once_with_or_without_an_episode() {
        for (channel, user_msg_id, episode) in [
            (7_533_402, 9_100_095_491_915_388_102, true),
            (7_533_401, 9_100_095_491_915_388_101, false),
        ] {
            let options = Options {
                episode,
                ..Options::default()
            };
            let fx = Fixture::new(channel, user_msg_id, options).await;
            // Queued without an enqueue kick: every kick counted below is the close's.
            let waiting = discord::relay_recovery::tests::orphan_token_finish::queued(channel + 1);
            let context = discord::queue_persistence_context(&fx.shared, &PROVIDER, fx.channel);
            let mailbox = fx.shared.mailbox(fx.channel);
            mailbox.replace_queue(vec![waiting], context).await;
            let kicks = Arc::new(Mutex::new(0_usize));
            let (seen, at) = (kicks.clone(), fx.channel);
            let _hook = discord::queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
                move |_, _, kicked, _| {
                    let ours = kicked == at;
                    if ours {
                        *seen.lock().unwrap() += 1;
                    }
                    let outcome = ours.then(discord::IdleQueueKickoffChannelOutcome::default);
                    Box::pin(async move { outcome })
                },
            ));
            let completion =
                discord::queue_io::cancel_backstop_test_support::KickoffCompletion::install(
                    fx.channel,
                );
            discord::spawn_turn_completion_idle_queue_listener(fx.shared.clone(), PROVIDER);
            completion.listener_ready(Duration::from_secs(10)).await;
            fx.pass().await;
            fx.assert_closed(&format!("episode={episode}")).await;
            completion
                .processed(usize::from(episode), Duration::from_secs(55))
                .await;
            assert_eq!(
                *kicks.lock().unwrap(),
                1,
                "episode={episode}: the close kicks once"
            );
            fx.pass().await;
            completion
                .processed(usize::from(episode), Duration::from_secs(55))
                .await;
            assert_eq!(
                *kicks.lock().unwrap(),
                1,
                "episode={episode}: a closed turn kicks no more"
            );
        }
    }
}
