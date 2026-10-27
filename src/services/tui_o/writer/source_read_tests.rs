//! Real hook ingress and channel log exercise source reads with production permission.
#![cfg(unix)]

use super::{
    actor,
    binding::{BindingEvents, ChannelBindingLog},
    owed::tests::owed_writer,
    *,
};
use crate::config::TestEnvVarGuard;
use crate::services::{
    claude_tui::hook_server::observation_ingress::observe_binding_hook,
    provider::CancelToken,
    tmux_common as tc,
    tui_o::{
        shadow::ShadowProvider,
        store::{ChannelStore, InitSource, Initialized, OStore, StoreConfig, ledger::PieceOutcome},
    },
    tui_prompt_dedupe::{
        self as dedupe,
        binding_context::{
            BINDING_HEADER, BindingContext, CapturedContext, HookBindingEnvelope,
            ObservedHookProcess, PreparedIncarnation,
        },
        binding_events,
    },
};
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::watch;

const A: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";
const B: &str = "019e660d-4859-7522-9cee-8ba7c4e7c744";
const PROMPT: &str = "read verified source";

struct Fixture {
    fake: crate::services::provider_teardown::tests::test_support::FakeTmux,
    context: BindingContext,
    root: tempfile::TempDir,
    _env: [TestEnvVarGuard; 2],
    _dedupe: std::sync::MutexGuard<'static, ()>,
    _shared: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

impl Fixture {
    fn new(verified: bool) -> Self {
        let shared = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (root, env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
        dedupe::reset_state_for_tests();
        binding_events::set_test_root(Some(root.path()));
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: if verified {
                crate::services::codex_tui::canary::CANARY_TMUX.into()
            } else {
                format!("legacy-{}", uuid::Uuid::new_v4().simple())
            },
            channel_id: Some(if verified {
                crate::services::codex_tui::canary::CANARY_CHANNEL
            } else {
                584_599
            }),
            owner_runtime_root: tc::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(sessions.canonicalize().unwrap()),
            first_prompt_digest: Some(format!("sha256:{:x}", Sha256::digest(PROMPT.as_bytes()))),
            source_policy: Some(if verified { "verified" } else { "legacy" }.into()),
        };
        PreparedIncarnation::create(context.clone()).unwrap();
        let marker = tc::session_temp_path(&context.tmux_session, "spawn_nonce");
        fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        fs::write(marker, &context.execution_nonce).unwrap();
        fs::write(
            tc::session_temp_path(&context.tmux_session, tc::TMUX_RUNTIME_KIND_TEMP_EXT),
            crate::services::agent_protocol::RuntimeHandoffKind::CodexTui.as_str(),
        )
        .unwrap();
        dedupe::register_tmux_channel(&context.tmux_session, context.channel_id.unwrap());
        let fake = crate::services::provider_teardown::tests::test_support::FakeTmux::install(
            &context.tmux_session,
        );
        Self {
            fake,
            context,
            root,
            _env: env,
            _dedupe: lock,
            _shared: shared,
        }
    }
    fn path(&self, id: &str) -> PathBuf {
        self.context
            .provider_root
            .as_ref()
            .unwrap()
            .join(format!("rollout-{id}.jsonl"))
    }
    fn header(&self, id: &str) {
        self.header_at(id, self.time(1));
    }
    fn header_at(&self, id: &str, time: DateTime<Utc>) {
        let time = time.to_rfc3339();
        let meta = json!({"type":"session_meta", "timestamp":time, "payload":{"id":id,"timestamp":time,"cwd":self.root.path(),"source":"cli","originator":"codex_cli_rs"}});
        fs::write(self.path(id), format!("{meta}\n")).unwrap();
    }
    fn send(
        &self,
        id: &str,
        event: &str,
        reason: &str,
        time: Option<DateTime<Utc>>,
        context: Option<BindingContext>,
    ) {
        let envelope = HookBindingEnvelope {
            context: CapturedContext::Captured(context.unwrap_or_else(|| self.context.clone())),
            observed: ObservedHookProcess::default(),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
        if let Some(time) = time {
            headers.insert(
                "x-agentdesk-relay-published-at",
                time.to_rfc3339().parse().unwrap(),
            );
        }
        let payload = json!({"session_id":id,"transcript_path":self.path(id),"source":reason,"prompt":PROMPT});
        let _ = observe_binding_hook("codex", event, Some(id), Some(id), &payload, &headers);
    }
    fn time(&self, n: i64) -> DateTime<Utc> {
        self.context.created_at + Duration::seconds(n)
    }
    fn log_bytes(&self) -> Vec<u8> {
        fs::read(
            self.root
                .path()
                .join(binding_events::BINDING_EVENTS_DIR)
                .join(format!("{}.log", self.context.channel_id.unwrap())),
        )
        .unwrap()
    }
    fn next_fresh(&mut self, created_at: DateTime<Utc>) -> BindingContext {
        let previous = self.context.clone();
        let log = self.log_bytes();
        self.context.created_at = created_at;
        self.context.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
        PreparedIncarnation::create(self.context.clone()).unwrap();
        fs::write(
            tc::session_temp_path(&self.context.tmux_session, "spawn_nonce"),
            &self.context.execution_nonce,
        )
        .unwrap();
        dedupe::reset_state_for_tests();
        dedupe::register_tmux_channel(&self.context.tmux_session, self.context.channel_id.unwrap());
        assert_eq!(
            self.log_bytes(),
            log,
            "new execution must retain durable history"
        );
        assert!(self.fold().verified.is_none());
        previous
    }
    fn fold(&self) -> binding_events::codex::Fold {
        binding_events::codex::read_ownership(&self.context).unwrap()
    }
    fn current(&self) -> dedupe::TuiRuntimeBinding {
        dedupe::runtime_binding_for_tmux_session(&self.context.tmux_session)
            .expect("production Unknown must read its verified source")
    }
    fn marker(&self) -> Vec<u8> {
        fs::read(tc::session_temp_path(
            &self.context.tmux_session,
            tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        ))
        .unwrap()
    }
    fn log(&self) -> ChannelBindingLog {
        ChannelBindingLog::new(self.context.channel_id.unwrap(), ShadowProvider::Codex)
    }
    fn store(&self) -> (OStore, ChannelStore) {
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, self.root.path())
            .unwrap()
            .unwrap();
        let source = self.fold().verified.unwrap().source;
        let channel = self.context.channel_id.unwrap();
        store
            .begin_era(&[channel], Utc::now(), |channel| {
                Ok(Initialized {
                    channel,
                    sources: vec![InitSource {
                        source_id: source.clone(),
                        delivery_start: 0,
                        prefix_hash: hex::encode(Sha256::digest(b"")),
                    }],
                    initial_anchor: 100,
                    build_digest: "source-read".into(),
                    at: Utc::now(),
                })
            })
            .unwrap();
        let era = store.read_era().unwrap().unwrap();
        let opened = store.open_channel(&era, channel).unwrap().unwrap();
        (store, opened)
    }
    fn reopened(&self, store: &OStore) -> ChannelStore {
        store
            .open_channel(
                &store.read_era().unwrap().unwrap(),
                self.context.channel_id.unwrap(),
            )
            .unwrap()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        binding_events::forget_channel_for_tests(self.context.channel_id.unwrap());
        binding_events::set_test_root(None);
        dedupe::reset_state_for_tests();
    }
}
fn append(path: &Path, text: &str) {
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}
fn unit(id: &str, text: &str) -> String {
    format!(
        "{}\n",
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","id":id,"content":[{"type":"output_text","text":text}]}})
    )
}
fn response(path: &Path, id: &str, text: &str) {
    append(path, &unit(id, text));
    append(
        path,
        &format!(
            "{}\n",
            json!({"type":"event_msg","payload":{"type":"task_complete","last_agent_message":text}})
        ),
    );
}
async fn polls() {
    tokio::time::sleep(actor::POLL_INTERVAL * 3).await;
}

async fn resume_older_native_header(delayed_descriptor: bool, prior_execution: bool) {
    let mut f = Fixture::new(true);
    let prior_context = prior_execution.then(|| {
        f.header(B);
        f.send(B, "session-start", "startup", Some(f.time(2)), None);
        assert_eq!(f.fold().verified.unwrap().source.session_id, B);
        assert_eq!(f.current().session_id.as_deref(), Some(B));
        f.next_fresh(f.time(1) + Duration::days(1))
    });
    f.header(A);
    f.send(A, "session-start", "startup", Some(f.time(2)), None);
    assert_eq!(f.fold().verified.unwrap().source.session_id, A);
    assert_eq!(f.current().session_id.as_deref(), Some(A));
    append(&f.path(A), &unit("a", "initial A"));
    let (store, opened) = f.store();
    let (writer, port) = owed_writer(opened);
    let (stop, stopped) = watch::channel(false);
    let (_command_port, receiver) = actor::codex_submission::Port::new(1);
    let task = actor::codex_submission::spawning(receiver, || {
        actor::spawn_projecting(
            &WriterConfig { enabled: true },
            None,
            writer,
            ShadowProvider::Codex,
            Arc::new(f.log()),
            (
                stopped,
                watch::channel(false).0,
                watch::channel(None).0,
                actor::Owing::default(),
            ),
        )
    })
    .unwrap();
    polls().await;
    assert_eq!(port.posts(), vec!["initial A"]);
    assert_eq!(posted(&f.reopened(&store)), 1);

    let old_header = f.context.created_at - Duration::days(1);
    let retained = f.path(B).with_extension("prior");
    if prior_execution && delayed_descriptor {
        fs::rename(f.path(B), &retained).unwrap();
    } else if !prior_execution && !delayed_descriptor {
        f.header_at(B, old_header);
    }
    if let Some(previous) = prior_context {
        let log = f.log_bytes();
        f.send(
            B,
            "session-start",
            "resume",
            Some(f.time(3)),
            Some(previous),
        );
        for time in [None, Some(f.time(2)), Some(f.time(1))] {
            f.send(B, "session-start", "resume", time, None);
        }
        assert_eq!(
            f.log_bytes(),
            log,
            "wrong nonce and unordered switches cannot append"
        );
        assert_eq!(f.current().session_id.as_deref(), Some(A));
    }
    f.send(B, "session-start", "resume", Some(f.time(3)), None);
    if delayed_descriptor {
        assert_eq!(f.fold().pending.len(), 1);
        assert!(dedupe::runtime_binding_for_tmux_session(&f.context.tmux_session).is_none());
        if prior_execution {
            fs::rename(&retained, f.path(B)).unwrap();
        } else {
            f.header_at(B, old_header);
        }
        dedupe::codex_verified::resolve_registered_claims();
    }
    if prior_execution {
        let log = f.log_bytes();
        binding_events::forget_channel_for_tests(f.context.channel_id.unwrap());
        dedupe::reset_state_for_tests();
        let replay = binding_events::codex::read_ownership(&f.context)
            .expect("durable B proof must replay after clearing only memory caches");
        assert_eq!(
            replay.verified.unwrap().source.session_id,
            B,
            "N1 must explicitly resume N0 B"
        );
        assert!(replay.pending.is_empty());
        assert_eq!(f.log_bytes(), log, "replay cannot rewrite durable history");
        dedupe::register_tmux_channel(&f.context.tmux_session, f.context.channel_id.unwrap());
        dedupe::codex_verified::resolve_registered_claims();
    }
    assert!(f.fold().pending.is_empty(), "older resume must resolve");
    assert_eq!(f.fold().verified.unwrap().source.session_id, B);
    assert_eq!(f.current().session_id.as_deref(), Some(B));
    assert!(!dedupe::codex_verified_input_blocked(
        &f.context.tmux_session
    ));
    append(&f.path(B), &unit("b", "older B"));
    polls().await;
    assert_eq!(port.posts(), vec!["initial A", "older B"]);
    assert_eq!(posted(&f.reopened(&store)), 2);

    f.send(A, "session-start", "resume", Some(f.time(4)), None);
    assert!(f.fold().pending.is_empty());
    assert_eq!(f.fold().verified.unwrap().source.session_id, A);
    assert_eq!(f.current().session_id.as_deref(), Some(A));
    append(&f.path(A), &unit("a-resumed", "resumed A"));
    polls().await;
    assert_eq!(port.posts(), vec!["initial A", "older B", "resumed A"]);
    assert_eq!(posted(&f.reopened(&store)), 3);
    stop.send(true).unwrap();
    assert!(task.await.unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn source_read_resume_older_native_header_posts_and_returns_to_a() {
    resume_older_native_header(false, false).await;
}

#[tokio::test(start_paused = true)]
async fn source_read_resume_older_native_header_pending_resolves_and_posts() {
    resume_older_native_header(true, false).await;
}

#[tokio::test(start_paused = true)]
async fn source_read_prior_execution_resume_posts_and_replays() {
    resume_older_native_header(false, true).await;
}

#[tokio::test(start_paused = true)]
async fn source_read_prior_execution_resume_pending_resolves_posts_and_replays() {
    resume_older_native_header(true, true).await;
}

#[test]
fn source_read_prior_execution_is_not_inherited_by_initial_fresh_proof() {
    let mut f = Fixture::new(true);
    f.header(B);
    f.send(B, "session-start", "startup", Some(f.time(2)), None);
    assert_eq!(f.fold().verified.unwrap().source.session_id, B);
    f.next_fresh(f.time(1) + Duration::days(1));
    // A renewed descriptor passes the creation-time floor but cannot inherit past ownership.
    f.header(B);
    f.send(B, "session-start", "startup", Some(f.time(2)), None);
    for _ in 0..2 {
        assert!(f.fold().verified.is_none());
        assert_eq!(f.fold().pending.len(), 1);
        assert!(dedupe::runtime_binding_for_tmux_session(&f.context.tmux_session).is_none());
        dedupe::codex_verified::resolve_registered_claims();
    }
}

#[test]
fn source_read_initial_fresh_older_native_header_stays_pending() {
    let f = Fixture::new(true);
    f.header_at(B, f.context.created_at - Duration::days(1));
    f.send(B, "session-start", "startup", Some(f.time(2)), None);
    for _ in 0..2 {
        assert!(f.fold().verified.is_none());
        assert_eq!(f.fold().pending.len(), 1);
        assert!(dedupe::runtime_binding_for_tmux_session(&f.context.tmux_session).is_none());
        assert!(dedupe::codex_verified_input_blocked(
            &f.context.tmux_session
        ));
        dedupe::codex_verified::resolve_registered_claims();
    }
}

#[tokio::test(start_paused = true)]
async fn source_read_unknown_hook_posts_and_follows_descriptor_compact_resume() {
    let f = Fixture::new(true);
    f.header(A);
    f.send(A, "session-start", "startup", Some(f.time(2)), None);
    assert_eq!(
        f.fold()
            .verified
            .as_ref()
            .expect("real hook must first persist a typed proof")
            .source
            .session_id,
        A
    );
    assert_eq!(f.current().session_id.as_deref(), Some(A));
    assert!(!dedupe::codex_verified_input_blocked(
        &f.context.tmux_session
    ));
    let source = f.fold().verified.unwrap().source;
    assert!(!f.marker().is_empty());
    let events = f
        .log()
        .binding_events_since(f.context.channel_id.unwrap(), 0)
        .unwrap();
    assert!(matches!(
        events[0].record,
        binding::BindingRecord::Bound { .. }
    ));
    append(&f.path(A), &unit("copy", "first"));
    let (store, opened) = f.store();
    let (writer, port) = owed_writer(opened);
    let (stop, stopped) = watch::channel(false);
    let (_command_port, receiver) = actor::codex_submission::Port::new(1);
    let task = actor::codex_submission::spawning(receiver, || {
        actor::spawn_projecting(
            &WriterConfig { enabled: true },
            None,
            writer,
            ShadowProvider::Codex,
            Arc::new(f.log()),
            (
                stopped,
                watch::channel(false).0,
                watch::channel(None).0,
                actor::Owing::default(),
            ),
        )
    })
    .unwrap();
    polls().await;
    assert_eq!(port.posts(), vec!["first"]);
    assert_eq!(posted(&f.reopened(&store)), 1);
    // A descriptor replacement keeps the UUID but must replace the proof and opened file.
    fs::rename(f.path(A), f.path(A).with_extension("old")).unwrap();
    f.header(A);
    append(
        &f.path(A),
        &(unit("copy", "first") + &unit("replacement", "replaced")),
    );
    f.send(A, "user-prompt-submit", "", Some(f.time(3)), None);
    assert_ne!(f.fold().verified.as_ref().unwrap().source.ino, source.ino);
    polls().await;
    assert_eq!(port.posts(), vec!["first", "replaced"]);
    f.send(B, "session-start", "compact", Some(f.time(4)), None);
    assert_eq!(f.fold().pending.len(), 1);
    f.header(B);
    append(
        &f.path(B),
        &(unit("copy", "first") + &unit("new-b", "compact")),
    );
    dedupe::codex_verified::resolve_registered_claims();
    assert!(f.fold().pending.is_empty());
    assert_eq!(f.current().session_id.as_deref(), Some(B));
    polls().await;
    assert_eq!(port.posts(), vec!["first", "replaced", "compact"]);
    f.send(A, "session-start", "resume", Some(f.time(5)), None);
    append(&f.path(A), &unit("resume", "resumed"));
    polls().await;
    assert_eq!(
        port.posts(),
        vec!["first", "replaced", "compact", "resumed"]
    );
    // The old Text proves both parser state and the following partial bytes were read.
    let start = fs::metadata(f.path(A)).unwrap().len();
    append(
        &f.path(A),
        &(unit("old-preview", "old preview") + "{\"type\":\"response_item\""),
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let (context, root, path, time) = (
        f.context.clone(),
        f.root.path().to_path_buf(),
        f.path(B),
        f.time(6),
    );
    let switch = std::thread::spawn(move || {
        let first = rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("old source must really emit before switching");
        assert!(format!("{first:?}").contains("old preview"), "{first:?}");
        binding_events::set_test_root(Some(&root));
        let envelope = HookBindingEnvelope {
            context: CapturedContext::Captured(context.clone()),
            observed: ObservedHookProcess::default(),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
        headers.insert(
            "x-agentdesk-relay-published-at",
            time.to_rfc3339().parse().unwrap(),
        );
        let payload =
            json!({"session_id":B,"transcript_path":path,"source":"resume","prompt":PROMPT});
        let observed = observe_binding_hook(
            "codex",
            "session-start",
            Some(B),
            Some(B),
            &payload,
            &headers,
        );
        assert!(!observed.refused(), "{observed:?}");
        assert_eq!(
            binding_events::codex::read_ownership(&context)
                .unwrap()
                .verified
                .unwrap()
                .source
                .session_id,
            B
        );
        response(&path, "tail-b", "tail switched");
        let mut frames = vec![first];
        frames.extend(rx);
        binding_events::set_test_root(None);
        frames
    });
    let token = Arc::new(CancelToken::new());
    let deadline = Arc::clone(&token);
    let (finished, wait) = std::sync::mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if wait
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_err()
        {
            deadline
                .cancelled
                .store(true, std::sync::atomic::Ordering::Release);
        }
    });
    let tail = crate::services::codex_tui::rollout_tail::tail_warm_followup_rollout_for_tmux(
        &f.path(A),
        start,
        A,
        tx,
        Some(Arc::clone(&token)),
        || true,
        &f.context.tmux_session,
        PROMPT,
    );
    let _ = finished.send(());
    watchdog.join().unwrap();
    let frames = switch.join().unwrap();
    assert!(
        !token.cancelled.load(std::sync::atomic::Ordering::Acquire),
        "tail watchdog is not success; {tail:?}; {frames:?}"
    );
    let tail = tail.unwrap();
    assert!(matches!(
        tail.read_result,
        crate::services::provider::ReadOutputResult::Completed { .. }
    ));
    assert_eq!(tail.rollout_path, f.path(B));
    assert_eq!(tail.session_id.as_deref(), Some(B));
    assert_eq!(tail.final_offset, fs::metadata(f.path(B)).unwrap().len());
    assert!(
        frames
            .iter()
            .any(|message| format!("{message:?}").contains("tail switched"))
    );
    assert!(frames.iter().any(|message| matches!(message, crate::services::agent_protocol::StreamMessage::CodexTuiTerminalDone { result, .. } if !result.contains("old preview") && result.contains("tail switched"))));
    immediate_followup(&f);
    polls().await;
    assert_eq!(
        port.posts()
            .iter()
            .filter(|p| p.as_str() == "first")
            .count(),
        1
    );
    stop.send(true).unwrap();
    assert!(task.await.unwrap().is_none());
    assert!(posted(&f.reopened(&store)) >= 5);
    assert!(matches!(
        f.reopened(&store).codex_provenance(),
        crate::services::tui_o::store::codex_provenance::Sidecar::Missing
    ));
}

#[tokio::test(start_paused = true)]
async fn source_read_wrong_nonce_or_uuid_never_posts_newer_same_cwd_source() {
    let f = Fixture::new(true);
    f.header(A);
    f.send(A, "session-start", "startup", Some(f.time(2)), None);
    f.header(B);
    append(&f.path(B), &unit("foreign", "wrong-session"));
    fs::File::options()
        .write(true)
        .open(f.path(B))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))
        .unwrap();
    let marker = f.marker();
    let before = f.fold();
    let mut bad = f.context.clone();
    bad.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
    f.send(B, "session-start", "resume", Some(f.time(3)), Some(bad));
    fs::write(f.path(B), fs::read(f.path(A)).unwrap()).unwrap();
    f.send(B, "session-start", "resume", Some(f.time(4)), None);
    assert_eq!(f.marker(), marker);
    assert_eq!(f.fold().verified, before.verified);
    let (_, opened) = f.store();
    let (writer, port) = owed_writer(opened);
    let (stop, stopped) = watch::channel(false);
    let task = actor::spawn_if_enabled(
        &WriterConfig { enabled: true },
        writer,
        ShadowProvider::Codex,
        Arc::new(f.log()),
        stopped,
        watch::channel(false).0,
    )
    .unwrap();
    polls().await;
    stop.send(true).unwrap();
    task.await.unwrap();
    assert!(port.posts().is_empty());
}

#[tokio::test(start_paused = true)]
async fn source_read_late_missing_descriptor_hook_preserves_marker_proof_cursor() {
    let f = Fixture::new(true);
    f.header(A);
    f.send(A, "session-start", "startup", Some(f.time(2)), None);
    let (store, opened) = f.store();
    let (writer, port) = owed_writer(opened);
    let (stop, stopped) = watch::channel(false);
    let task = actor::spawn_if_enabled(
        &WriterConfig { enabled: true },
        writer,
        ShadowProvider::Codex,
        Arc::new(f.log()),
        stopped,
        watch::channel(false).0,
    )
    .unwrap();
    polls().await;
    f.header(B);
    f.send(B, "session-start", "compact", Some(f.time(4)), None);
    polls().await;
    let native_offset = fs::metadata(f.path(B)).unwrap().len();
    assert!(dedupe::advance_tmux_runtime_binding_offset(
        &f.context.tmux_session,
        f.path(B).to_str().unwrap(),
        native_offset
    ));
    let before = f.fold();
    let marker = f.marker();
    let current = f.current();
    let source = before.verified.as_ref().unwrap().source.clone();
    let offset = f.reopened(&store).cursor(&source).unwrap().captured_through;
    for missing in [false, true] {
        if missing {
            fs::rename(f.path(A), f.path(A).with_extension("gone")).unwrap();
        }
        f.send(A, "user-prompt-submit", "", Some(f.time(3)), None);
        f.send(A, "session-start", "resume", Some(f.time(3)), None);
        assert_eq!(f.fold(), before);
        assert_eq!(f.marker(), marker);
        assert_eq!(f.current(), current);
        polls().await;
        assert_eq!(
            f.reopened(&store).cursor(&source).unwrap().captured_through,
            offset
        );
        assert!(f.fold().pending.is_empty());
    }
    append(&f.path(B), &unit("after-stale", "B continues"));
    polls().await;
    assert_eq!(port.posts(), vec!["B continues"]);
    stop.send(true).unwrap();
    task.await.unwrap();
}

#[test]
fn source_read_transition_missing_equal_and_regressing_time_is_fail_closed() {
    let f = Fixture::new(true);
    f.header(A);
    f.send(A, "session-start", "startup", Some(f.time(2)), None);
    f.header(B);
    let fold = f.fold();
    let marker = f.marker();
    let current = f.current();
    for time in [None, Some(f.time(1)), Some(f.time(2))] {
        f.send(B, "session-start", "resume", time, None);
        assert_eq!(f.fold(), fold);
        assert_eq!(f.marker(), marker);
        assert_eq!(f.current(), current);
    }
    f.send(B, "session-start", "resume", Some(f.time(3)), None);
    f.send(A, "session-start", "resume", Some(f.time(4)), None);
    assert_eq!(f.current().session_id.as_deref(), Some(A));
    assert!(f.fold().pending.is_empty());
    let log = f
        .root
        .path()
        .join(binding_events::BINDING_EVENTS_DIR)
        .join(format!("{}.log", f.context.channel_id.unwrap()));
    let original = fs::read(&log).unwrap();
    let rows: Vec<serde_json::Value> = String::from_utf8(original.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 3);
    for time in [
        serde_json::Value::Null,
        json!(f.time(1).to_rfc3339()),
        json!(f.time(2).to_rfc3339()),
    ] {
        let mut corrupt = rows.clone();
        corrupt[1]["codex_ownership"]["published_at"] = time;
        fs::write(
            &log,
            corrupt
                .iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>(),
        )
        .unwrap();
        assert!(binding_events::codex::read_ownership(&f.context).is_err());
        assert!(dedupe::runtime_binding_for_tmux_session(&f.context.tmux_session).is_none());
        fs::write(&log, &original).unwrap();
    }
    let mut corrupt = rows.clone();
    corrupt[1]["old"] = corrupt[1]["new"]["source"].clone();
    fs::write(
        &log,
        corrupt
            .iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>(),
    )
    .unwrap();
    assert!(binding_events::codex::read_ownership(&f.context).is_err());
    fs::write(&log, &original).unwrap();
    let mut legacy = rows;
    legacy[0]["codex_ownership"]
        .as_object_mut()
        .unwrap()
        .remove("published_at");
    fs::write(
        &log,
        legacy
            .iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>(),
    )
    .unwrap();
    assert_eq!(
        f.current().session_id.as_deref(),
        Some(A),
        "legacy received_at supplies the migration floor"
    );
    fs::write(&log, original).unwrap();
}

#[test]
fn source_read_legacy_selector_keeps_latest_mtime_and_resume_boundary() {
    let f = Fixture::new(false);
    // The scan returns paths under CODEX_HOME; resolve it like `f.path` so macOS `/var` matches.
    let home = f.root.path().canonicalize().unwrap();
    let _home = TestEnvVarGuard::set_path_after_shared_test_env_lock("CODEX_HOME", &home);
    f.header(A);
    response(&f.path(A), "old", "older");
    f.header(B);
    response(&f.path(B), "new", "newest");
    for (id, seconds) in [(A, 10), (B, 20)] {
        fs::File::options()
            .write(true)
            .open(f.path(id))
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds))
            .unwrap();
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let result = crate::services::codex_tui::rollout_tail::tail_latest_rollout_for_cwd_with_handoff_for_tmux(
        f.root.path(), std::time::UNIX_EPOCH, tx, None, || true, &f.context.tmux_session, None,
    ).unwrap();
    assert_eq!(result.rollout_path, f.path(B));
    assert!(
        rx.try_iter()
            .any(|message| format!("{message:?}").contains("newest"))
    );
    assert!(!dedupe::codex_verified_requires_proof(
        &f.context.tmux_session
    ));
    assert_eq!(
        super::past_bind::first_boundary(binding::BindingCause::Resume, false, false),
        crate::services::tui_o::store::rotation::Boundary::Pending {
            candidates: vec![0]
        }
    );
    assert!(
        f.fake
            .take_calls()
            .iter()
            .all(|call| !call.starts_with("kill-"))
    );
}

fn immediate_followup(f: &Fixture) {
    use crate::services::codex_tui::{
        host_input::spy::{SpyGuard, SpyState},
        warm_followup,
    };
    use std::os::unix::fs::PermissionsExt;
    let binary = f.root.path().join("codex");
    fs::write(&binary, "#!/bin/sh\nif [ \"$1 $2\" = 'resume --help' ]; then echo --dangerously-bypass-hook-trust; else echo 'codex-cli 0.157.1'; fi\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let _binary =
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_CODEX_PATH", &binary);
    let _hooks = TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_DIRECT_TUI_HOOKS",
        std::ffi::OsStr::new("1"),
    );
    let _endpoint = crate::services::claude_tui::hook_server::publish_hook_endpoint(
        "http://127.0.0.1:9".into(),
    );
    let options = crate::services::codex::actual_submit_tests::options(B, f.root.path());
    warm_followup::write_codex_tui_launch_options_evidence(&f.context.tmux_session, &options)
        .unwrap();
    let ready = "╭──────────────────────────────────────────────╮\n│ ▌                                            │\n╰──────────────────────────────────────────────╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send";
    let draft = "╭──────────────────────────────────────────────╮\n│ first ▌                                      │\n╰──────────────────────────────────────────────╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send";
    let path = f.path(B);
    let spy = SpyGuard::install(SpyState {
        captures: [ready, draft, ready, ready]
            .into_iter()
            .map(|s| Some(s.into()))
            .collect(),
        on_enter: Some(Box::new(move || {
            response(&path, "immediate", "immediately injected")
        })),
        ..SpyState::default()
    });
    let token = Arc::new(CancelToken::new());
    let deadline = Arc::clone(&token);
    let (finished, wait) = std::sync::mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if wait
            .recv_timeout(std::time::Duration::from_secs(3))
            .is_err()
        {
            deadline
                .cancelled
                .store(true, std::sync::atomic::Ordering::Release);
        }
    });
    let (tx, _rx) = std::sync::mpsc::channel();
    let result = warm_followup::verified_warm(warm_followup::VerifiedWarmRequest {
        tmux: &f.context.tmux_session,
        channel: f.context.channel_id.unwrap(),
        session_id: B,
        cwd: f.root.path(),
        prompt: "first",
        options: &options,
        token: Arc::clone(&token),
        sender: tx,
    });
    let _ = finished.send(());
    watchdog.join().unwrap();
    assert!(
        !token.cancelled.load(std::sync::atomic::Ordering::Acquire),
        "input watchdog is not success"
    );
    assert!(result.is_ok(), "{result:?}; {:?}", spy.calls());
    let calls = spy.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| matches!(call.as_str(), "literal:first" | "load:first"))
            .count(),
        1,
        "{calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.as_str() == "keys:Enter")
            .count(),
        1,
        "{calls:?}"
    );
    assert!(spy.calls().iter().all(|call| !call.contains("kill")));
}

fn posted(store: &ChannelStore) -> usize {
    let ledger = store.ledger();
    (0..ledger.next_serial())
        .filter_map(|serial| ledger.piece(serial))
        .filter(|piece| matches!(piece.outcome, Some(PieceOutcome::Posted(_))))
        .count()
}

#[path = "executed_resume_tests.rs"]
mod executed_resume;
