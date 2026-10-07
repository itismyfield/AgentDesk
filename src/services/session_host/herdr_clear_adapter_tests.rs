//! The Herdr clear adapter run by the native clear helper, over an in-process Herdr socket, a PG
//! row, the execution's launch context and a scratch binding log.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::config::{TestEnvVarGuard as Guard, TestRuntimeRootGuard};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedLookup, HostedLookupKey, HostedOwner, SourceRef,
    load_hosted_execution_pg,
};
use crate::services::herdr_admission::{Admission, ForcedAdmission, force_for_test};
use crate::services::session_host::herdr_registry::ForcedRegistry;
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use crate::services::tmux_common::{session_temp_path, with_tmux_source_authority};
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};
use crate::services::tui_prompt_dedupe::binding_events::{
    self, BindingCause, CauseSource, HookSignal, Proposal, SourceId,
};
use crate::services::tui_prompt_dedupe::native_clear::{
    ClearAdmission, ClearOutcome, NativeClearRestart, judge_native_clear_restart,
    start_native_clear,
};

const CHANNEL: u64 = 1_479_671_301_387_059_402;
const TOKEN: &str = "discord_0123456789abcdef";
const HOST: &str = "test-node";
const EMPTY_COMPOSER: &str = "Claude Code v2.1.141\n\n\u{276f} \nstatus";
const DRAFT: &str = "\u{276f} 남은 초안 한글";

fn open_admission() -> ForcedAdmission {
    let stop = std::env::temp_dir().join(format!("adk-hc-none-{}", uuid::Uuid::new_v4()));
    force_for_test(Admission::new(None, Some(stop)))
}

/// The session side of a clear, counting what the adapter asked of it.
#[derive(Clone, Default)]
struct Session {
    selector_clears: Arc<AtomicUsize>,
    saved: Arc<Mutex<Vec<String>>>,
}

impl ClearSession for Session {
    fn clear_selector(&mut self) -> Effect<'_> {
        self.selector_clears.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { true })
    }

    fn save(&mut self, commit: ClearCommit) -> Effect<'_> {
        self.saved.lock().unwrap().push(commit.session);
        Box::pin(async { true })
    }
    fn finish(&mut self, _: ClearCommit) -> Effect<'_> {
        Box::pin(async { true })
    }
}

/// The helper runs its host on a blocking thread; admission is installed there as on this one.
struct OnWorker<H> {
    host: H,
    admission: Option<ForcedAdmission>,
}

impl<H: NativeClearHost> NativeClearHost for OnWorker<H> {
    fn changes(&self) -> watch::Receiver<u64> {
        self.host.changes()
    }
    fn prepare(&mut self, deadline: Instant) -> Effect<'_> {
        self.admission.get_or_insert_with(open_admission);
        self.host.prepare(deadline)
    }
    fn submit(&mut self, deadline: Instant) -> NativeClearSubmission {
        self.host.submit(deadline)
    }
    fn decide(&mut self, allow_fallback: bool) -> ClearDecision {
        self.host.decide(allow_fallback)
    }
    fn composer_empty(&mut self, deadline: Instant) -> bool {
        self.host.composer_empty(deadline)
    }
    fn save(&mut self, commit: ClearCommit, deadline: Instant) -> Effect<'_> {
        self.host.save(commit, deadline)
    }
    fn finish(&mut self, commit: ClearCommit, deadline: Instant) -> Effect<'_> {
        self.host.finish(commit, deadline)
    }
    fn fallback(&mut self, deadline: Instant) -> Effect<'_> {
        self.host.fallback(deadline)
    }
}

/// A Bound Claude execution on the rig's pane: its PG row, launch context, nonce and host markers,
/// and a binding log whose verified source is `old`.
struct Fixture {
    rt: tokio::runtime::Runtime,
    db: Option<crate::db::auto_queue::test_support::TestPostgresDb>,
    pool: sqlx::PgPool,
    key: String,
    logical: String,
    nonce: String,
    context: PathBuf,
    rig: HerdrRig,
    log: tempfile::TempDir,
    _thread: (ForcedRegistry, ForcedAdmission),
    _env: (Guard, TestRuntimeRootGuard),
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = TestRuntimeRootGuard::new();
        let instance = Guard::set_value_after_shared_test_env_lock(
            "AGENTDESK_INSTANCE_ID",
            HOST.as_ref() as &std::ffi::OsStr,
        );
        // The node identity comes from this root's config, never the operator's.
        let config =
            crate::runtime_layout::config_file_path(&crate::config::runtime_root().unwrap());
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, format!("cluster:\n  instance_id: {HOST}\n")).unwrap();
        assert_eq!(
            crate::services::tui_prompt_dedupe::binding_context::stable_host_identity().as_deref(),
            Some(HOST)
        );
        let log = tempfile::tempdir().unwrap();
        binding_events::set_test_root(Some(log.path()));
        let rig = HerdrRig::start();
        let thread = (rig.registry_on_this_thread(), open_admission());
        let logical = format!("AgentDesk-claude-p9b4-{tag}");
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let context = BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: nonce.clone(),
            tmux_session: logical.clone(),
            channel_id: Some(CHANNEL),
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
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let key = format!("claude/{TOKEN}/{NODE}:{logical}");
        let record = serde_json::to_value(bound(&logical, &nonce, &rig)).unwrap();
        let (db, pool) = rt.block_on(async {
            let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            sqlx::query(
                "INSERT INTO sessions (session_key, provider, status, identity_kind,
                                       discord_token_hash, channel_id, hosted_execution)
                 VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)",
            )
            .bind(&key)
            .bind(TOKEN)
            .bind(CHANNEL.to_string())
            .bind(record)
            .execute(&pool)
            .await
            .unwrap();
            (db, pool)
        });
        let fixture = Self {
            rt,
            db: Some(db),
            pool,
            key,
            logical,
            nonce,
            context,
            rig,
            log,
            _thread: thread,
            _env: (instance, root),
        };
        fixture.record("old", BindingCause::Startup, false);
        fixture
    }

    fn bound(&self) -> HostedExecution {
        bound(&self.logical, &self.nonce, &self.rig)
    }

    /// The row as the turn host reads it.
    fn row(&self) -> HostedRecord {
        let lookup = HostedLookupKey::SessionKey(&self.key);
        match self
            .rt
            .block_on(load_hosted_execution_pg(&self.pool, lookup))
        {
            HostedLookup::Found(observed) => observed.record,
            other => panic!("{other:?}"),
        }
    }

    fn source(&self, session: &str) -> SourceId {
        SourceId {
            session_id: session.into(),
            path: self.log.path().join(format!("{session}.jsonl")),
            dev: 1,
            ino: session.bytes().map(u64::from).sum(),
        }
    }

    /// A SessionStart(clear)-shaped record of `session`, logged as the hook path logs it.
    fn record(&self, session: &str, cause: BindingCause, pending: bool) {
        let source = self.source(session);
        let path = source.path.display().to_string();
        let hook = HookSignal::from_payload("session_start", &json!({"source": "clear"}));
        let proposal = Proposal {
            channel_id: CHANNEL,
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

    fn hold(&self) -> PathBuf {
        let root = crate::config::runtime_root().unwrap();
        root.join("runtime/herdr_input_holds").join(&self.nonce)
    }

    fn marker_names_execution(&self) -> bool {
        std::fs::read_to_string(session_temp_path(&self.logical, "spawn_nonce"))
            .is_ok_and(|marker| marker == self.nonce)
    }

    /// Plans the clear on the row read from PG with `unsettled` as O's projection.
    fn plan(&self, unsettled: Option<usize>) -> Result<HerdrClearPlan, HerdrClearRefusal> {
        plan_clear(CHANNEL, Some(&self.row()), unsettled)
    }

    /// Runs a planned clear in the helper; `hook` plays the provider once `/clear` was sent.
    fn run(&self, plan: HerdrClearPlan, session: &Session, hook: impl FnOnce()) -> ClearOutcome {
        let host = OnWorker {
            host: HerdrClear::new(plan, session.clone()),
            admission: None,
        };
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let guard = self.rt.block_on(lock.clone().lock_owned());
        let mut receiver = {
            let _runtime = self.rt.enter();
            start_native_clear(host, ClearAdmission::Native, guard)
        };
        let outcome = loop {
            if let Ok(outcome) = receiver.try_recv() {
                break outcome;
            }
            if !self.rig.sends().is_empty() {
                hook();
                break self.rt.block_on(receiver).unwrap();
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(lock.try_lock().is_ok(), "the worker released its guard");
        outcome
    }

    /// The whole clear as its caller drives it: a refusal changes nothing.
    fn clear(
        &self,
        unsettled: Option<usize>,
        session: &Session,
        hook: impl FnOnce(),
    ) -> Result<ClearOutcome, HerdrClearRefusal> {
        let plan = self.plan(unsettled)?;
        Ok(self.run(plan, session, hook))
    }

    fn assert_untouched(&self, session: &Session, what: &str) {
        assert_eq!(
            self.rig.sends(),
            Vec::<Value>::new(),
            "{what}: nothing sent"
        );
        assert_eq!(
            session.selector_clears.load(Ordering::SeqCst),
            0,
            "{what}: selector kept"
        );
        assert!(self.marker_names_execution(), "{what}: execution kept");
        assert!(
            matches!(self.row(), HostedRecord::Known(r) if r.state == HostedState::Bound),
            "{what}: row Bound"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        binding_events::forget_channel_for_tests(CHANNEL);
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

/// A Bound execution of `nonce` on the rig's pane.
fn bound(logical: &str, nonce: &str, rig: &HerdrRig) -> HostedExecution {
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: TOKEN.into(),
        channel_id: CHANNEL.to_string(),
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

fn restart_boundary(plan_ticket: &Value) -> crate::db::session_transcripts::NativeClearBoundary {
    crate::db::session_transcripts::NativeClearBoundary::Unresolved {
        generation: crate::db::session_transcripts::NativeClearGeneration(1),
        ticket: plan_ticket.clone(),
    }
}

// T-C1/T-C2: one gated `/clear` line, committed by its own SessionStart(clear) Pending, saves the
// cleared session and keeps the pane, execution and row; a restart finds the same commit.
#[test]
fn a_pending_clear_commits_with_one_line_and_keeps_the_pane_and_row_pg() {
    let fx = Fixture::new("commit");
    let session = Session::default();
    let plan = fx
        .plan(Some(0))
        .expect("a Bound matched execution plans its clear");
    let ticket = serde_json::to_value(&plan.waiter().ticket).unwrap();
    let outcome = fx.run(plan, &session, || {
        fx.record("cleared", BindingCause::Clear, true)
    });
    assert!(fx.marker_names_execution(), "the execution is kept");
    assert_eq!(fx.rig.sends(), clear_line(), "one gated line, never again");
    assert_eq!(
        *session.saved.lock().unwrap(),
        ["cleared"],
        "the cleared session"
    );
    assert_eq!(session.selector_clears.load(Ordering::SeqCst), 1);
    assert!(matches!(fx.row(), HostedRecord::Known(r) if r.state == HostedState::Bound));
    let ClearOutcome::Native(commit) = outcome else {
        panic!("a durable Pending commit is a native clear: {outcome:?}");
    };
    assert_eq!(commit.session, "cleared");
    assert!(commit.source.is_none(), "committed on the Pending");
    let restarted = judge_native_clear_restart(&restart_boundary(&ticket), Some(HOST));
    assert_eq!(restarted, NativeClearRestart::CompleteDurable(commit));
}

// T-C3: a clear event logged before the baseline, or one of another execution after the send,
// commits nothing: the helper holds at its deadline with no second line and the pane kept.
#[test]
fn an_earlier_or_foreign_clear_event_holds_without_a_second_line_pg() {
    let fx = Fixture::new("hold");
    fx.record("earlier", BindingCause::Clear, true);
    fx.record("old", BindingCause::Startup, false);
    let session = Session::default();
    let plan = fx.plan(Some(0)).unwrap();
    let ticket = serde_json::to_value(&plan.waiter().ticket).unwrap();
    let outcome = fx.run(plan, &session, || {
        std::fs::write(
            session_temp_path(&fx.logical, "spawn_nonce"),
            "f".repeat(32),
        )
        .unwrap();
        fx.record("foreign", BindingCause::Clear, true);
        std::fs::write(session_temp_path(&fx.logical, "spawn_nonce"), &fx.nonce).unwrap();
    });
    assert_eq!(fx.rig.sends(), clear_line(), "no second line");
    assert!(session.saved.lock().unwrap().is_empty(), "nothing saved");
    assert!(fx.marker_names_execution(), "no reset or kill");
    assert!(matches!(fx.row(), HostedRecord::Known(r) if r.state == HostedState::Bound));
    assert_eq!(outcome, ClearOutcome::Hold(None));
    let restarted = judge_native_clear_restart(&restart_boundary(&ticket), Some(HOST));
    assert_eq!(restarted, NativeClearRestart::ResetUnresolved);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(CHANNEL, NODE)]);
    assert!(
        crate::services::discord::admin_host_guard::configured_refusal(CHANNEL).is_some(),
        "a restart's unresolved clear on a configured channel refuses the managed reset"
    );
}

// T-C4: admission off, an unverified E7 server, a replaced execution or a row that is not Bound
// is refused before any line or selector change.
#[test]
fn every_failed_check_refuses_before_any_change_pg() {
    let fx = Fixture::new("refuse");
    let session = Session::default();
    {
        let stop = std::env::temp_dir().join(format!("adk-hc-stop-{}", uuid::Uuid::new_v4()));
        std::fs::write(&stop, "").unwrap();
        let _off = force_for_test(Admission::new(None, Some(stop)));
        let refused = fx.clear(Some(0), &session, || {}).err();
        fx.assert_untouched(&session, "admission off");
        assert!(
            matches!(
                refused,
                Some(HerdrClearRefusal::Gate(HerdrGateRefusal::AdmissionStopped(
                    _
                )))
            ),
            "{refused:?}"
        );
    }
    fx.rig.serve_as(&[7, 8]);
    let refused = fx.clear(Some(0), &session, || {}).err();
    fx.assert_untouched(&session, "E7 on another server");
    assert!(
        matches!(refused, Some(HerdrClearRefusal::Gate(_))),
        "{refused:?}"
    );
    fx.rig.serve_as(&[7]);
    fx.rig.run_provider(&fx.context, true);
    let refused = fx.clear(Some(0), &session, || {}).err();
    fx.assert_untouched(&session, "identity mismatch");
    assert!(
        matches!(refused, Some(HerdrClearRefusal::Gate(_))),
        "{refused:?}"
    );
    let mut pending = fx.bound();
    pending.state = HostedState::Pending;
    let refused = plan_clear(CHANNEL, Some(&HostedRecord::Known(pending)), Some(0)).err();
    assert_eq!(
        refused,
        Some(HerdrClearRefusal::NotBound(Some(HostedState::Pending)))
    );
    assert_eq!(
        plan_clear(CHANNEL, None, Some(0)).err(),
        Some(HerdrClearRefusal::NotBound(None))
    );
    fx.assert_untouched(&session, "not Bound");
}

// A prompt that may sit unsubmitted in the composer holds the clear: `/clear` would be submitted
// with it. The hold stays; only a retire ends it.
#[test]
fn an_input_hold_refuses_the_clear_and_stays_pg() {
    let fx = Fixture::new("held");
    let session = Session::default();
    std::fs::create_dir_all(fx.hold().parent().unwrap()).unwrap();
    std::fs::write(fx.hold(), "2026-10-05T00:00:00Z").unwrap();
    let refused = fx.clear(Some(0), &session, || {}).err();
    fx.assert_untouched(&session, "held");
    assert!(fx.hold().exists(), "the hold is kept");
    assert_eq!(refused, Some(HerdrClearRefusal::InputHeld));
}

// T-C6/T-C7/T-C9: while O still reads a source an earlier rotation left, or its projection is
// unread, the clear is refused before any change, whatever the log's latest record is.
#[test]
fn an_unsettled_or_unread_rotation_refuses_the_clear_pg() {
    let fx = Fixture::new("rotation");
    let session = Session::default();
    fx.record("cleared", BindingCause::Clear, true);
    fx.record("cleared", BindingCause::Clear, false);
    for (unsettled, refusal) in [
        (Some(1), HerdrClearRefusal::RotationUnsettled(1)),
        (None, HerdrClearRefusal::RotationUnread),
    ] {
        let refused = fx.clear(unsettled, &session, || {}).err();
        fx.assert_untouched(&session, &format!("{unsettled:?}"));
        assert_eq!(refused, Some(refusal));
    }
}

// A draft left in the composer is never submitted with `/clear`: nothing is sent and the clear
// holds without a reset.
#[test]
fn a_draft_in_the_composer_sends_nothing_and_holds_pg() {
    let fx = Fixture::new("draft");
    fx.rig.answer("pane.read", screen(DRAFT));
    let session = Session::default();
    let outcome = fx.clear(Some(0), &session, || {}).unwrap();
    assert_eq!(fx.rig.sends(), Vec::<Value>::new(), "nothing sent");
    assert!(fx.marker_names_execution(), "no reset or kill");
    assert_eq!(outcome, ClearOutcome::Hold(None));
}

// An unclear `/clear` (reply lost after the server took it, or composer not seen cleared) is never
// sent again: it holds at its deadline, pane kept, and a later Pending settles it at restart.
#[test]
fn an_unclear_clear_line_holds_without_a_second_and_its_late_pending_settles_pg() {
    for (tag, reply_lost) in [("lost-reply", true), ("composer-unseen", false)] {
        let fx = Fixture::new(tag);
        match reply_lost {
            true => fx.rig.leave_sends_unanswered(true),
            false => fx.rig.answer_after_send("pane.read", screen(DRAFT)),
        }
        let session = Session::default();
        let plan = fx.plan(Some(0)).unwrap();
        let ticket = serde_json::to_value(&plan.waiter().ticket).unwrap();
        let outcome = fx.run(plan, &session, || {});
        assert_eq!(fx.rig.sends(), clear_line(), "{tag}: one line, never again");
        assert!(
            session.saved.lock().unwrap().is_empty(),
            "{tag}: nothing saved"
        );
        assert!(fx.marker_names_execution(), "{tag}: no reset or kill");
        assert_eq!(outcome, ClearOutcome::Hold(None), "{tag}");

        fx.record("late", BindingCause::Clear, true);
        let restarted = judge_native_clear_restart(&restart_boundary(&ticket), Some(HOST));
        assert!(
            matches!(&restarted, NativeClearRestart::CompleteDurable(c) if c.session == "late"),
            "{tag}: the late Pending settles the clear: {restarted:?}"
        );
        assert_eq!(fx.rig.sends(), clear_line(), "{tag}: still one line");
    }
}
