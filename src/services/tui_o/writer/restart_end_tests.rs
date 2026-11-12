#![cfg(unix)]

use std::future::Future;
use std::io::Write;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, watch};

use super::{POLL_INTERVAL, run_channel};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId, UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::{LedgerEntry, PieceOutcome};
use crate::services::tui_o::store::restart_end::{self, RestartEnd};
use crate::services::tui_o::store::{ChannelStore, InitSource, Initialized, OStore, StoreConfig};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvents, BindingEvidence, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::deliver::{ChannelWriter, StopCause};
use crate::services::tui_o::writer::{
    AlarmSink, DeliveryLease, DiscordPort, PostOutcome, SeenMessage, WriterAlarm,
};

const BOT: u64 = 42;
const PANE: &str = "fixture-restart-pane";
const OLD_BODY: &str = "old partial";
const NEW_BODY: &str = "new answer";
const NEW_SUMMARY: &str = "-# ⏱️ 7초";

#[derive(Default)]
struct Port {
    posts: Mutex<Vec<String>>,
    history: Mutex<Vec<SeenMessage>>,
    after_post: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    posted: Notify,
}

impl Port {
    fn record_post(&self, content: String) -> SeenMessage {
        self.posts.lock().unwrap().push(content.clone());
        let mut history = self.history.lock().unwrap();
        let message = SeenMessage {
            id: 1000 + history.len() as u64,
            author_id: BOT,
            content,
        };
        history.push(message.clone());
        message
    }
    fn posted(&self) -> Vec<String> {
        self.posts.lock().unwrap().clone()
    }
    async fn wait_for_posts(&self, count: usize) {
        loop {
            let posted = self.posted.notified();
            if self.posts.lock().unwrap().len() >= count {
                return;
            }
            posted.await;
        }
    }
}

impl DiscordPort for Port {
    fn bot_id(&self) -> u64 {
        BOT
    }
    fn post(&self, _: u64, content: String) -> impl Future<Output = PostOutcome> + Send + 'static {
        let message = self.record_post(content);
        let after_post = self.after_post.lock().unwrap().take();
        if let Some(after_post) = after_post {
            after_post();
        }
        self.posted.notify_waiters();
        async move { PostOutcome::Created(message) }
    }
    async fn history_after(&self, _: u64, after: u64) -> Result<Vec<SeenMessage>, String> {
        Ok(self
            .history
            .lock()
            .unwrap()
            .iter()
            .filter(|message| message.id > after)
            .cloned()
            .collect())
    }
    async fn message(&self, _: u64, id: u64) -> Result<Option<SeenMessage>, String> {
        Ok(self
            .history
            .lock()
            .unwrap()
            .iter()
            .find(|message| message.id == id)
            .cloned())
    }
}

struct Lease;
impl DeliveryLease for Lease {
    type Held = ();
    fn try_acquire(&self, _: u64, _: u64) -> Option<()> {
        Some(())
    }
}

#[derive(Clone, Default)]
struct Alarms(Arc<Mutex<Vec<WriterAlarm>>>);
impl AlarmSink for Alarms {
    fn raise(&self, _: u64, alarm: WriterAlarm) {
        self.0.lock().unwrap().push(alarm);
    }
}

struct Bindings {
    events: Vec<BindingEvent>,
    changed: watch::Sender<u64>,
}
impl BindingEvents for Bindings {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        Ok(self
            .events
            .iter()
            .filter(|event| event.channel_id == channel && event.seq > after)
            .cloned()
            .collect())
    }
    fn subscribe(&self, _: u64) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }
}

type Actor = tokio::task::JoinHandle<Option<StopCause>>;

fn native(value: Value) -> Vec<u8> {
    let mut raw = serde_json::to_vec(&value).unwrap();
    raw.push(b'\n');
    raw
}

fn prompt(id: &str, timestamp: &str) -> Vec<u8> {
    native(json!({"type":"user","uuid":id,"timestamp":timestamp,
        "message":{"role":"user","content":"continue the work"}}))
}

fn body(id: &str, text: &str) -> Vec<u8> {
    native(
        json!({"type":"assistant","uuid":format!("row-{id}"),"apiBlockIndex":0,
        "message":{"id":id,"role":"assistant","content":[{"type":"text","text":text}]}}),
    )
}

fn key(channel: u64, native_key: &str, kind: UnitKind) -> UnitKey {
    UnitKey {
        channel_id: channel,
        provider: ShadowProvider::Claude,
        native_key: native_key.into(),
        kind,
    }
}

struct Harness {
    channel: u64,
    runtime: tempfile::TempDir,
    store: OStore,
    port: Arc<Port>,
    gate: Arc<OwnershipGate>,
    alarms: Alarms,
    bindings: Arc<Bindings>,
}

impl Harness {
    fn new(channel: u64) -> Self {
        Self::build(channel, false)
    }
    fn build(channel: u64, late_end: bool) -> Self {
        let runtime = tempfile::tempdir().unwrap();
        let path = runtime.path().join("native.jsonl");
        let opener = prompt("old-question", "2026-11-12T00:00:00Z");
        let old = [opener.clone(), body("old-body", OLD_BODY)].concat();
        std::fs::write(&path, &old).unwrap();
        let source = source_id_for("same-native-session", &path).unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
            .unwrap()
            .unwrap();
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
                    build_digest: "fixture".into(),
                    at: Utc::now(),
                })
            })
            .unwrap();
        let witness = RestartEnd {
            end_id: format!("effort-{channel}"),
            channel,
            provider: ShadowProvider::Claude,
            tmux_session: PANE.into(),
            execution_nonce: "a".repeat(32),
            binding_seq: 1,
            source: source.clone(),
            cutoff: old.len() as u64,
            prefix_hash: hex::encode(Sha256::digest(&old)),
            open_after: opener.len() as u64,
            native_turn_id: Some("old-question".into()),
        };
        let new = [prompt("new-question", "2026-11-12T00:01:00Z"), body("new-body", NEW_BODY),
            native(json!({"type":"system","subtype":"stop_hook_summary","timestamp":"2026-11-12T00:01:07Z"}))].concat();
        let port = Arc::<Port>::default();
        let publish = {
            let runtime = runtime.path().to_owned();
            move || {
                assert_eq!(
                    restart_end::append(&runtime, &witness),
                    Ok(restart_end::Appended::Durable)
                );
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(&new)
                    .unwrap();
            }
        };
        if late_end {
            *port.after_post.lock().unwrap() = Some(Box::new(publish));
        } else {
            publish();
        }
        let events = vec![
            Self::bound(channel, 1, &source, false),
            Self::bound(channel, 2, &source, true),
        ];
        let bindings = Arc::new(Bindings {
            events,
            changed: watch::channel(2).0,
        });
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        Self {
            channel,
            runtime,
            store,
            port,
            gate,
            alarms: Alarms::default(),
            bindings,
        }
    }
    fn bound(channel: u64, seq: u64, source: &SourceId, resumed: bool) -> BindingEvent {
        BindingEvent {
            seq,
            channel_id: channel,
            provider: ShadowProvider::Claude,
            tmux_session: PANE.into(),
            execution_nonce: if resumed {
                "b".repeat(32)
            } else {
                "a".repeat(32)
            },
            record: BindingRecord::Bound {
                old: resumed.then(|| source.clone()),
                new: BindingTarget::Source(source.clone()),
                cause: if resumed {
                    BindingCause::Resume
                } else {
                    BindingCause::Startup
                },
                parent_hint: None,
                evidence: BindingEvidence {
                    hook_event: "SessionStart".into(),
                    received_at: Utc::now(),
                    reclaims: false,
                },
            },
            committed_at: Utc::now(),
        }
    }
    fn channel(&self) -> ChannelStore {
        self.store
            .open_channel(&self.store.read_era().unwrap().unwrap(), self.channel)
            .unwrap()
            .unwrap()
    }
    fn start(&self) -> (watch::Sender<bool>, Actor, watch::Receiver<bool>) {
        let writer = ChannelWriter::new(
            self.channel(),
            self.gate.clone(),
            self.port.clone(),
            Lease,
            self.alarms.clone(),
        );
        let (stop, stopped) = watch::channel(false);
        let (resumed, ready) = watch::channel(false);
        let task = tokio::spawn(run_channel(
            writer,
            ShadowProvider::Claude,
            self.bindings.clone(),
            stopped,
            resumed,
        ));
        (stop, task, ready)
    }
}

async fn finish(stop: watch::Sender<bool>, task: Actor, mut ready: watch::Receiver<bool>) {
    ready
        .wait_for(|ready| *ready)
        .await
        .expect("the real actor must finish its boot replay before timed polls");
    // Restored Prepared waits for its first history lookup before later pieces may drain.
    let polls = super::super::confirm::FIRST_LOOK.as_secs() / POLL_INTERVAL.as_secs() + 8;
    for _ in 0..polls {
        tokio::time::advance(POLL_INTERVAL).await;
        stop.send_replace(false);
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }
    stop.send_replace(true);
    assert!(task.await.unwrap().is_none());
}

async fn exercise(channel: u64, prepared: bool) {
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    crate::config_live_reload::install(crate::config::Config {
        tui_o: Some(crate::services::tui_o::shadow::tap::TuiOConfig::default()),
        ..crate::config::Config::default()
    });
    crate::services::tui_prompt_dedupe::codex_verified::provenance::admission::set_ready_for_tests(
        channel, false,
    );
    let harness = Harness::new(channel);
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        harness.runtime.path(),
    );
    if prepared {
        let mut channel_store = harness.channel();
        channel_store
            .append_ledger(LedgerEntry::Excluded {
                unit_key: key(channel, "old-body:0#turn", UnitKind::Tool),
                reason: "turn_summary_open".into(),
            })
            .unwrap();
        channel_store
            .append_ledger(LedgerEntry::Prepared {
                serial: 0,
                unit_key: key(channel, "old-body:0", UnitKind::Body),
                piece_index: 0,
                payload: OLD_BODY.into(),
                anchor_id: 100,
                epoch: 1,
                local_retry: None,
                notice_origin: None,
            })
            .unwrap();
        harness.port.record_post(OLD_BODY.into());
    }
    let (stop, actor, ready) = harness.start();
    finish(stop, actor, ready).await;
    let alarms = harness.alarms.0.lock().unwrap().clone();
    assert!(alarms.is_empty(), "{alarms:?}");
    let restored = harness.channel();
    let ledger = restored.ledger();
    assert!(
        ledger.summary_turn_marks().contains("new-body:0#turn"),
        "the new prompt must open a separate summary turn after the physical end"
    );
    assert!(ledger.summary_turn_marks().contains("old-body:0#turn"));
    assert!(
        ledger
            .latest_piece(&key(channel, "old-body:0#turn_end", UnitKind::Tool), 0)
            .is_none(),
        "a physical end must not invent a completed native summary"
    );
    let (_, summary) = ledger
        .latest_piece(&key(channel, "new-body:0#turn_end", UnitKind::Tool), 0)
        .expect("the new native turn owns its summary");
    assert_eq!(
        summary.payload, NEW_SUMMARY,
        "the old prompt's timestamp must not leak into the new turn"
    );
    assert!(matches!(summary.outcome, Some(PieceOutcome::Posted(_))));
    assert_eq!(harness.port.posted(), [OLD_BODY, NEW_BODY, NEW_SUMMARY]);
    assert_eq!(ledger.next_serial(), 3);
    if prepared {
        let (serial, old) = ledger
            .latest_piece(&key(channel, "old-body:0", UnitKind::Body), 0)
            .unwrap();
        assert_eq!(serial, 0);
        assert_eq!(old.outcome, Some(PieceOutcome::Posted(1000)));
    }
    drop(restored);
    let (stop, actor, ready) = harness.start();
    finish(stop, actor, ready).await;
    assert_eq!(
        harness.port.posted(),
        [OLD_BODY, NEW_BODY, NEW_SUMMARY],
        "boot replay must retain the same posted identities"
    );
}

#[tokio::test(start_paused = true)]
async fn witness_is_applied_before_seal() {
    exercise(810_921_101, false).await;
}

#[tokio::test(start_paused = true)]
async fn prepared_post_survives_restart_without_duplicate() {
    exercise(810_921_102, true).await;
}

#[tokio::test(start_paused = true)]
async fn an_end_published_during_post_precedes_the_next_live_capture() {
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    crate::config_live_reload::install(crate::config::Config {
        tui_o: Some(crate::services::tui_o::shadow::tap::TuiOConfig::default()),
        ..crate::config::Config::default()
    });
    let channel = 810_921_103;
    crate::services::tui_prompt_dedupe::codex_verified::provenance::admission::set_ready_for_tests(
        channel, false,
    );
    let harness = Harness::build(channel, true);
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        harness.runtime.path(),
    );
    assert!(
        restart_end::read(harness.runtime.path(), channel)
            .unwrap()
            .is_empty()
    );
    let (stop, actor, mut ready) = harness.start();
    ready.wait_for(|ready| *ready).await.unwrap();
    harness.port.wait_for_posts(3).await;
    stop.send_replace(true);
    assert!(actor.await.unwrap().is_none());

    assert!(harness.alarms.0.lock().unwrap().is_empty());
    let restored = harness.channel();
    let ledger = restored.ledger();
    assert_eq!(
        restart_end::read(harness.runtime.path(), channel)
            .unwrap()
            .len(),
        1
    );
    assert!(ledger.summary_turn_marks().contains("new-body:0#turn"));
    assert!(
        ledger
            .latest_piece(&key(channel, "old-body:0#turn_end", UnitKind::Tool), 0)
            .is_none()
    );
    let (_, summary) = ledger
        .latest_piece(&key(channel, "new-body:0#turn_end", UnitKind::Tool), 0)
        .expect("the next native turn owns its summary even when the end arrives inside POST");
    assert_eq!(summary.payload, NEW_SUMMARY);
    assert!(matches!(summary.outcome, Some(PieceOutcome::Posted(_))));
    assert_eq!(harness.port.posted(), [OLD_BODY, NEW_BODY, NEW_SUMMARY]);
    assert_eq!(ledger.next_serial(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_warm_actor_keeps_readiness_while_restart_storage_blocks_capture() {
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    crate::config_live_reload::install(crate::config::Config {
        tui_o: Some(crate::services::tui_o::shadow::tap::TuiOConfig::default()),
        ..crate::config::Config::default()
    });
    let channel = 810_921_104;
    crate::services::tui_prompt_dedupe::codex_verified::provenance::admission::set_ready_for_tests(
        channel, false,
    );
    let harness = Harness::build(channel, true);
    let publish = harness.port.after_post.lock().unwrap().take().unwrap();
    let runtime = harness.runtime.path().to_owned();
    *harness.port.after_post.lock().unwrap() = Some(Box::new(move || {
        publish();
        let directory = runtime
            .join(restart_end::DIR_NAME)
            .join(channel.to_string());
        std::fs::rename(&directory, runtime.join("held-end-records")).unwrap();
        std::fs::write(directory, b"not a readable directory").unwrap();
    }));
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        harness.runtime.path(),
    );
    let (stop, actor, mut ready) = harness.start();
    ready.wait_for(|ready| *ready).await.unwrap();
    assert!(
        *ready.borrow(),
        "the writer must first become ready without any end"
    );
    harness.port.wait_for_posts(1).await;
    for _ in 0..2 {
        tokio::time::advance(POLL_INTERVAL).await;
        stop.send_replace(false);
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }
    let stayed_ready = *ready.borrow();
    let posted = harness.port.posted();
    stop.send_replace(true);
    assert!(actor.await.unwrap().is_none());

    assert!(
        restart_end::read(harness.runtime.path(), channel).is_err(),
        "the fixture must leave a directory-wide read error"
    );
    assert!(
        stayed_ready,
        "a warm writer retains readiness while retrying witness storage"
    );
    assert_eq!(
        posted,
        [OLD_BODY],
        "unverified new native records stay uncaptured"
    );
    assert_eq!(harness.channel().ledger().next_serial(), 1);
}
