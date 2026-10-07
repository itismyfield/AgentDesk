//! `!clear` on a Herdr-configured channel from the clear entry: the Herdr native clear over an
//! in-process Herdr socket and PG, with the native clear switch left off.
#![cfg(unix)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use self::serenity::{ChannelId, MessageId, UserId};
use super::*;
use crate::config::{TestEnvVarGuard as Guard, TestRuntimeRootGuard};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedOwner, HostedRecord, HostedState, SourceRef,
};
use crate::db::session_transcripts::native_channel_clear_state;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::discord::{CancelToken, DiscordSession};
use crate::services::herdr_admission::{Admission, ForcedAdmission, force_for_test};
use crate::services::session_backend::{SessionHandle, insert_process_session};
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use crate::services::tmux_common::{session_temp_path, with_tmux_source_authority};
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};
use crate::services::tui_prompt_dedupe::binding_events::{
    self, BindingCause, CauseSource, HookSignal, Proposal, SourceId,
};

const HOST: &str = "test-node";
const EMPTY_COMPOSER: &str = "Claude Code v2.1.141\n\n\u{276f} \nstatus";

fn open_admission() -> ForcedAdmission {
    let stop = std::env::temp_dir().join(format!("adk-hc-none-{}", uuid::Uuid::new_v4()));
    force_for_test(Admission::new(None, Some(stop)))
}

/// The selector effects, recorded in order; a save may fail.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    save_fails: AtomicBool,
}

impl Fake {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl NativeClearEffects for Fake {
    fn clear_selector<'a>(&'a self, session_key: &'a str) -> Effect<'a> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("clear:{session_key}"));
        Box::pin(async { true })
    }
    fn save_selector<'a>(&'a self, _: &'a str, session: &'a str, _: ChannelId) -> Effect<'a> {
        self.calls.lock().unwrap().push(format!("save:{session}"));
        let ok = !self.save_fails.load(Ordering::SeqCst);
        Box::pin(async move { ok })
    }
    fn submit(&self, _: &ClearTicket, _: Instant) -> NativeClearSubmission {
        self.calls.lock().unwrap().push("tmux-submit".into());
        NativeClearSubmission::NotSent
    }
    fn composer_empty(&self, _: &str, _: Instant) -> bool {
        true
    }
    fn reset_process(&self, tmux: &str) {
        self.calls.lock().unwrap().push(format!("reset:{tmux}"));
    }
}

/// A Herdr-configured channel's Bound execution on the rig's pane, its PG row, markers, O readiness
/// and binding log; every runtime thread sees the rig's registry, open admission and that log.
struct Fixture {
    rt: tokio::runtime::Runtime,
    db: Option<crate::db::auto_queue::test_support::TestPostgresDb>,
    pool: sqlx::PgPool,
    shared: Arc<SharedData>,
    http: Arc<serenity::Http>,
    channel_id: ChannelId,
    channel_name: String,
    logical: String,
    session_key: String,
    nonce: String,
    rig: Arc<HerdrRig>,
    log: tempfile::TempDir,
    fake: Arc<Fake>,
    alive: Arc<AtomicBool>,
    _thread: Vec<Box<dyn std::any::Any>>,
    _env: (Guard, TestRuntimeRootGuard),
}

impl Fixture {
    fn new(n: u64) -> Self {
        let root = TestRuntimeRootGuard::new();
        let instance = Guard::set_value_after_shared_test_env_lock(
            "AGENTDESK_INSTANCE_ID",
            HOST.as_ref() as &std::ffi::OsStr,
        );
        let runtime_root = crate::config::runtime_root().unwrap();
        let config = crate::runtime_layout::config_file_path(&runtime_root);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, format!("cluster:\n  instance_id: {HOST}\n")).unwrap();
        let log = tempfile::tempdir().unwrap();
        let rig = Arc::new(HerdrRig::start());
        let rt = {
            let (rig, log) = (rig.clone(), log.path().to_path_buf());
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .on_thread_start(move || {
                    std::mem::forget(rig.registry_on_this_thread());
                    std::mem::forget(open_admission());
                    binding_events::set_test_root(Some(&log));
                })
                .build()
                .unwrap()
        };
        binding_events::set_test_root(Some(log.path()));
        let channel_id = ChannelId::new(1_479_671_303_387_100_000 + n);
        let channel_name = format!("adk-p9b4-s2-{n}-{}", uuid::Uuid::new_v4().simple());
        let logical = ProviderKind::Claude.build_tmux_session_name(&channel_name);
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let context = BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: nonce.clone(),
            tmux_session: logical.clone(),
            channel_id: Some(channel_id.get()),
            owner_runtime_root: "test".into(),
            host: Some(HOST.into()),
            expected_native_session_id: Some("old".into()),
            launch_mode: "fresh".into(),
            provider_root: None,
            first_prompt_digest: None,
            source_policy: None,
        };
        let context = PreparedIncarnation::create(context).unwrap().path;
        std::fs::write(session_temp_path(&logical, "spawn_nonce"), &nonce).unwrap();
        std::fs::write(session_temp_path(&logical, "host_kind"), "herdr").unwrap();
        rig.run_provider(&context, false);
        rig.answer("pane.read", screen(EMPTY_COMPOSER));
        let (store, era) =
            crate::services::herdr_launch::o_store_for_test(&runtime_root, &[channel_id.get()]);
        let mut seeded = store.open_channel(&era, channel_id.get()).unwrap().unwrap();
        seeded.set_binding_checkpoint(3).unwrap();
        // The managed reset would end this process; a Herdr clear never reaches it.
        let alive = Arc::new(AtomicBool::new(true));
        insert_process_session(
            logical.clone(),
            SessionHandle::TestProcess {
                pid: 5_340_904,
                alive: alive.clone(),
            },
        );
        let fake = Arc::new(Fake::default());
        let thread: Vec<Box<dyn std::any::Any>> = vec![
            Box::new(rig.registry_on_this_thread()),
            Box::new(open_admission()),
            Box::new(crate::config::session_hosts::force_for_test(
                Some(NODE),
                &[(channel_id.get(), NODE)],
            )),
            Box::new(crate::services::turn_host::force_switch_for_test(Some(
                true,
            ))),
            Box::new(crate::services::herdr_launch::force_writer_accepts(Some(
                true,
            ))),
            Box::new(
                crate::services::tui_o::cutover::test_override::force_channels(&[(
                    channel_id.get(),
                    ClaudeTui,
                )]),
            ),
            Box::new(crate::services::tui_o::writer::host::force_unsettled_for_test(Some(0))),
            Box::new(host_effects_for_tests(fake.clone())),
        ];
        let (db, pool, shared, session_key) = rt.block_on(async {
            let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
                pool.clone(),
            ));
            shared.core.lock().await.sessions.insert(
                channel_id,
                session(channel_id, channel_name.clone(), &shared),
            );
            let build = super::super::super::super::adk_session::build_namespaced_session_key;
            let session_key = build(&shared.token_hash, &ProviderKind::Claude, &logical);
            let record = bound(&logical, &nonce, &rig, &shared.token_hash, channel_id);
            sqlx::query(
                "INSERT INTO sessions (session_key, provider, status, identity_kind,
                                       discord_token_hash, channel_id, hosted_execution)
                 VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)",
            )
            .bind(&session_key)
            .bind(&shared.token_hash)
            .bind(channel_id.get().to_string())
            .bind(serde_json::to_value(record).unwrap())
            .execute(&pool)
            .await
            .unwrap();
            (db, pool, shared, session_key)
        });
        let fixture = Self {
            rt,
            db: Some(db),
            pool,
            shared,
            http: Arc::new(serenity::Http::new("")),
            channel_id,
            channel_name,
            logical,
            session_key,
            nonce,
            rig,
            log,
            fake,
            alive,
            _thread: thread,
            _env: (instance, root),
        };
        fixture.record("old", BindingCause::Startup, false);
        fixture
    }

    /// A SessionStart(clear)-shaped record of `session`, logged as the hook path logs it.
    fn record(&self, session: &str, cause: BindingCause, pending: bool) {
        let source = SourceId {
            session_id: session.into(),
            path: self.log.path().join(format!("{session}.jsonl")),
            dev: 1,
            ino: session.bytes().map(u64::from).sum(),
        };
        let path = source.path.display().to_string();
        let hook = HookSignal::from_payload("session_start", &json!({"source": "clear"}));
        let proposal = Proposal {
            channel_id: self.channel_id.get(),
            provider: "claude",
            tmux_session: &self.logical,
            session_id: Some(session),
            path: &path,
            replaced: None,
            cause: CauseSource::Hook(cause),
            hook: Some(&hook),
        };
        with_tmux_source_authority(&self.logical, |_| {
            if pending {
                binding_events::record_pending(&proposal).unwrap();
            } else {
                binding_events::record_verified(&proposal, &source).unwrap();
            }
        });
    }

    async fn clear(&self) -> anyhow::Result<()> {
        super::super::clear_channel_session_state(
            &self.http,
            &self.shared,
            &ProviderKind::Claude,
            self.channel_id,
            "!clear",
            super::super::SoftClearNotifyMode::Suppress,
        )
        .await
    }

    /// The clear, with `hook` playing the pane's SessionStart(clear) once `/clear` was sent.
    async fn clear_with(&self, hook: impl FnOnce()) -> anyhow::Result<()> {
        let mut clear = Box::pin(self.clear());
        let mut hook = Some(hook);
        loop {
            tokio::select! {
                result = &mut clear => return result,
                () = tokio::time::sleep(Duration::from_millis(10)) => {
                    if !self.rig.sends().is_empty() && let Some(hook) = hook.take() {
                        hook();
                    }
                }
            }
        }
    }

    async fn admits(&self) -> (bool, Option<String>) {
        let mut state = (Some("stale".to_string()), true, String::new());
        let admitted = native_clear_admits(
            &self.http,
            &self.shared,
            &ProviderKind::Claude,
            self.channel_id,
            &mut state,
            &mut false,
        )
        .await;
        (admitted, state.0)
    }

    async fn state(&self) -> NativeClearBoundary {
        native_channel_clear_state(&self.pool, &self.channel_id.get().to_string())
            .await
            .unwrap()
    }

    async fn session(&self) -> (Option<String>, bool) {
        let data = self.shared.core.lock().await;
        let session = data.sessions.get(&self.channel_id).unwrap();
        (session.session_id.clone(), session.cleared)
    }

    async fn row_bound(&self) -> bool {
        let raw: Option<Value> =
            sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE session_key = $1")
                .bind(&self.session_key)
                .fetch_one(&self.pool)
                .await
                .unwrap();
        matches!(
            HostedRecord::decode(raw.as_ref()),
            HostedRecord::Known(record)
                if record.state == HostedState::Bound && record.execution_nonce == self.nonce
        )
    }

    fn marker_names_execution(&self) -> bool {
        std::fs::read_to_string(session_temp_path(&self.logical, "spawn_nonce"))
            .is_ok_and(|marker| marker == self.nonce)
    }

    /// Nothing of the pane, its execution, its process or the selector moved.
    async fn assert_untouched(&self, what: &str) {
        assert_eq!(
            self.rig.sends(),
            Vec::<Value>::new(),
            "{what}: nothing sent"
        );
        assert_eq!(
            self.fake.calls(),
            Vec::<String>::new(),
            "{what}: selector kept"
        );
        assert!(
            self.alive.load(Ordering::SeqCst),
            "{what}: no managed reset"
        );
        assert!(self.marker_names_execution(), "{what}: execution kept");
        assert!(self.row_bound().await, "{what}: row Bound");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        binding_events::forget_channel_for_tests(self.channel_id.get());
        binding_events::set_test_root(None);
        if let Some(db) = self.db.take() {
            let pool = self.pool.clone();
            self.rt.block_on(async {
                pool.close().await;
                db.drop().await;
            });
        }
    }
}

fn session(channel_id: ChannelId, channel_name: String, shared: &SharedData) -> DiscordSession {
    DiscordSession {
        session_id: Some("old".into()),
        memento_context_loaded: true,
        memento_reflected: false,
        current_path: None,
        history: Vec::new(),
        pending_uploads: Vec::new(),
        cleared: false,
        remote_profile_name: None,
        channel_id: Some(channel_id.get()),
        channel_name: Some(channel_name),
        category_name: None,
        last_active: tokio::time::Instant::now(),
        worktree: None,
        born_generation: shared.restart.current_generation,
    }
}

/// A Bound execution of `nonce` on the rig's pane.
fn bound(
    logical: &str,
    nonce: &str,
    rig: &HerdrRig,
    token_hash: &str,
    channel_id: ChannelId,
) -> HostedExecution {
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: token_hash.into(),
        channel_id: channel_id.get().to_string(),
        logical_key: logical.into(),
        owner_node: NODE.into(),
        runtime_root: "/adk/runtime".into(),
    };
    HostedExecution {
        schema: 1,
        state: HostedState::Bound,
        execution_nonce: nonce.into(),
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            named_session: SESSION.into(),
            pane_id: PANE.into(),
        }),
        expected: Some(rig.expected(nonce)),
        source_ref: SourceRef {
            runtime_root: owner.runtime_root.clone(),
            channel: owner.channel_id.clone(),
            provider: "claude".into(),
            logical_key: logical.into(),
            execution_nonce: nonce.into(),
            initial_source: None,
            baseline_event_seq: None,
        },
        owner,
    }
}

fn screen(text: &str) -> Value {
    json!({"type": "pane_read", "read": {
        "pane_id": PANE, "workspace_id": "w1", "tab_id": "w1:1", "source": "recent_unwrapped",
        "format": "text", "text": text, "revision": 3, "truncated": false
    }})
}

fn clear_line() -> Vec<Value> {
    vec![json!({"pane_id": PANE, "text": "/clear", "keys": ["enter"]})]
}

// T-C1/T-C2: a Bound execution clears through one gated `/clear` line, its own Pending commits
// it, the cleared session is saved and the pane, execution, row and process stay.
#[test]
fn a_configured_channel_clears_its_pane_with_one_line_and_saves_the_pending_session_pg() {
    let fixture = Fixture::new(1);
    fixture.rt.block_on(async {
        let result = fixture
            .clear_with(|| fixture.record("new", BindingCause::Clear, true))
            .await;

        let key = &fixture.session_key;
        assert_eq!(
            fixture.rig.sends(),
            clear_line(),
            "one gated line, never again"
        );
        assert_eq!(
            fixture.fake.calls(),
            [format!("clear:{key}"), "save:new".into()],
            "the selector is cleared before `/clear`, then the cleared session saved"
        );
        assert!(fixture.alive.load(Ordering::SeqCst), "no managed reset");
        assert!(fixture.marker_names_execution(), "the execution is kept");
        assert!(fixture.row_bound().await, "the row stays Bound");
        assert_eq!(fixture.state().await, NativeClearBoundary::Resolved);
        assert_eq!(fixture.session().await, (Some("new".into()), true));
        result.expect("the clear completes once");
    });
}

// T-C2 restart: a commit whose save failed holds the clear and its input; the next admission,
// native switch off, completes it from the durable Pending with no second line and no reset.
#[test]
fn a_held_herdr_clear_completes_at_admission_without_a_second_line_pg() {
    let fixture = Fixture::new(2);
    fixture.rt.block_on(async {
        fixture.fake.save_fails.store(true, Ordering::SeqCst);
        let result = fixture
            .clear_with(|| fixture.record("new", BindingCause::Clear, true))
            .await;

        assert!(
            matches!(
                fixture.state().await,
                NativeClearBoundary::Unresolved { .. }
            ),
            "a failed save leaves the clear unresolved"
        );
        assert_eq!(fixture.admits().await.0, false, "input is held");
        assert!(result.is_err(), "a held clear is not success");

        fixture.fake.save_fails.store(false, Ordering::SeqCst);
        assert_eq!(fixture.admits().await, (true, Some("new".into())));
        assert_eq!(fixture.state().await, NativeClearBoundary::Resolved);
        assert_eq!(fixture.rig.sends(), clear_line(), "no second line");
        let resets = fixture
            .fake
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("reset:"));
        assert_eq!(resets.count(), 0, "no reset");
        assert!(fixture.alive.load(Ordering::SeqCst), "no managed reset");
        assert!(fixture.marker_names_execution(), "the execution is kept");
    });
}

// T-C8: the helper holds the channel's transition guard until the commit is judged and the
// session saved; a second `!clear` meanwhile waits, and admission reopens on a settled clear.
#[test]
fn the_herdr_clear_holds_the_channel_until_its_commit_is_settled_pg() {
    let fixture = Fixture::new(3);
    fixture.rt.block_on(async {
        let lock = fixture.shared.session_transition_lock(fixture.channel_id);
        let mut first = Box::pin(fixture.clear());
        while fixture.rig.sends().is_empty() {
            tokio::select! {
                result = &mut first => panic!("the clear ended before `/clear`: {result:?}"),
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
        assert!(lock.try_lock().is_err(), "the guard is held after `/clear`");
        let second = tokio::time::timeout(Duration::from_millis(300), fixture.clear()).await;
        if let Ok(second) = second {
            assert!(
                second.is_err(),
                "a second `!clear` waits or is refused: {second:?}"
            );
        }

        fixture.record("new", BindingCause::Clear, true);
        drop(first);
        let admitted = lock.lock_owned().await;
        assert_eq!(
            fixture.state().await,
            NativeClearBoundary::Resolved,
            "the commit is settled before the guard is released"
        );
        assert_eq!(fixture.session().await, (Some("new".into()), true));
        assert_eq!(
            fixture.rig.sends(),
            clear_line(),
            "the waiting clear sent nothing"
        );
        drop(admitted);
    });
}

// D2: no Herdr stop reaches a running turn, so a clear is refused before the queue, the selector
// or the pane changes, and the turn keeps running.
#[test]
fn a_running_turn_refuses_the_herdr_clear_before_any_change_pg() {
    let fixture = Fixture::new(4);
    fixture.rt.block_on(async {
        let before = fixture.state().await;
        let token = Arc::new(CancelToken::new());
        let started = crate::services::discord::mailbox_try_start_turn(
            &fixture.shared,
            fixture.channel_id,
            token.clone(),
            UserId::new(1),
            MessageId::new(5_340_904),
        );
        assert!(started.await, "the turn holds the mailbox");

        let result = fixture.clear().await;

        fixture.assert_untouched("running turn").await;
        assert_eq!(fixture.state().await, before, "no boundary");
        assert!(
            !token.cancelled.load(Ordering::SeqCst),
            "the turn is not stopped"
        );
        let mailbox = fixture.shared.mailbox(fixture.channel_id);
        assert!(
            mailbox.has_active_turn().await.unwrap(),
            "the turn keeps the mailbox"
        );
        assert_eq!(fixture.session().await, (Some("old".into()), false));
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("herdr clear refused: turn_in_progress"),
            "{error}"
        );
    });
}

// Without a database the entry refuses as main does, and the fenced clear refuses before any
// change; switched off, a configured clear gets main's refusal. None reaches the managed reset.
#[test]
fn a_configured_clear_without_its_database_or_switch_changes_nothing_pg() {
    let fixture = Fixture::new(5);
    fixture.rt.block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let session = session(fixture.channel_id, fixture.channel_name.clone(), &shared);
        shared
            .core
            .lock()
            .await
            .sessions
            .insert(fixture.channel_id, session);
        let clear = |fenced: bool| {
            let shared = &shared;
            let (http, channel_id) = (&fixture.http, fixture.channel_id);
            let (claude, quiet) = (
                &ProviderKind::Claude,
                super::super::SoftClearNotifyMode::Suppress,
            );
            async move {
                match fenced {
                    true => {
                        let fenced = super::super::clear_channel_session_state_fenced;
                        fenced(http, shared, claude, channel_id, "!clear", quiet, None).await
                    }
                    false => {
                        let entry = super::super::clear_channel_session_state;
                        entry(http, shared, claude, channel_id, "!clear", quiet).await
                    }
                }
            }
        };
        let entry = clear(false).await;
        fixture.assert_untouched("no database at the entry").await;
        let error = entry.unwrap_err().to_string();
        assert!(
            error.contains("postgres pool is required"),
            "main's refusal: {error}"
        );
        let fenced = clear(true).await;
        fixture.assert_untouched("no database").await;
        let error = fenced.unwrap_err().to_string();
        assert!(
            error.contains("herdr clear refused: no_database"),
            "{error}"
        );

        let _off = crate::services::turn_host::force_switch_for_test(Some(false));
        let off = fixture.clear().await;
        fixture.assert_untouched("switched off").await;
        let error = off.unwrap_err().to_string();
        assert!(error.contains("Herdr 설정 채널"), "main's refusal: {error}");
    });
}

// A `!clear` planned while an earlier one ran plans again under the guard: the earlier Pending is
// not its commit, so it sends its own line and commits on its own hook; the next turn takes it.
#[test]
fn a_clear_waiting_on_the_guard_commits_only_on_its_own_pending_pg() {
    let fixture = Fixture::new(6);
    fixture.rt.block_on(async {
        let mut first = Box::pin(fixture.clear());
        while fixture.rig.sends().is_empty() {
            tokio::select! {
                result = &mut first => panic!("the first clear ended before `/clear`: {result:?}"),
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
        let pins = || {
            let requests = fixture.rig.requests();
            let pins = requests
                .iter()
                .filter(|r| r["method"] == "pane.process_info");
            pins.count()
        };
        let before = pins();
        let mut second = Box::pin(fixture.clear());
        while pins() == before {
            tokio::select! {
                result = &mut second => panic!("ended before its plan: {result:?}"),
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
        let waiting = tokio::time::timeout(Duration::from_millis(300), &mut second).await;
        assert!(
            waiting.is_err(),
            "the second clear waits on the guard: {waiting:?}"
        );

        fixture.record("y", BindingCause::Clear, true);
        first
            .await
            .expect("the first clear commits on its own Pending");
        while fixture.rig.sends().len() < 2 {
            tokio::select! {
                result = &mut second => panic!("ended before its line: {result:?}"),
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
        let early = tokio::time::timeout(Duration::from_millis(500), &mut second).await;
        assert!(
            early.is_err(),
            "the first clear's Pending is not its commit: {early:?}"
        );
        let key = &fixture.session_key;
        let (clear, save) = (format!("clear:{key}"), |s: &str| format!("save:{s}"));
        assert_eq!(
            fixture.fake.calls(),
            [clear.clone(), save("y"), clear.clone()],
            "nothing saved for the second clear yet"
        );
        assert!(
            matches!(
                fixture.state().await,
                NativeClearBoundary::Unresolved { .. }
            ),
            "its boundary waits"
        );

        fixture.record("z", BindingCause::Clear, true);
        second
            .await
            .expect("the second clear commits on its own Pending");
        let lines = [clear_line(), clear_line()].concat();
        assert_eq!(fixture.rig.sends(), lines, "one line each, never again");
        assert_eq!(
            fixture.fake.calls(),
            [clear.clone(), save("y"), clear, save("z")]
        );
        assert_eq!(fixture.state().await, NativeClearBoundary::Resolved);
        assert_eq!(fixture.session().await, (Some("z".into()), true));
        let channel = fixture.channel_id.get();
        let next = crate::services::claude::herdr_turn::awaited_clear;
        let next = next(channel, &fixture.logical, &fixture.nonce).map(|a| a.session_id);
        assert_eq!(
            next.as_deref(),
            Some("z"),
            "the next turn takes the latest clear"
        );
    });
}

#[path = "native_herdr_e2e_tests.rs"]
mod e2e;
