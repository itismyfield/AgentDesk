//! Terminal rows through the writer poll: a stored interrupt barrier and a raw EOF closer.
use super::*;
use crate::services::session_host::herdr_socket_rig_tests::HerdrRig;
use crate::services::tui_o::shadow::capture::Fingerprint;
use crate::services::tui_o::writer::{
    actor::native_input::{EvidencePort, NativePort, Snapshot, Terminal},
    input_facts::native::{
        Barrier, Projection, Registration, registrations_fingerprint,
        terminal::{RawWitness, Tail},
    },
    input_notices::NoticeOrigin,
};
use std::fs::{self, File};
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;

async fn stored_interrupt_barrier_replays(boot: bool) {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::tui_o::writer::input_facts::native::{Barrier, registrations_fingerprint};
    let mark = "[Request interrupted by user]";
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let root = runtime.path().join("runtime");
    let f = Fixture::at(root.join("injected_frames.json"), 81480481, true);
    let rows = [
        serde_json::json!({"type":"user","uuid":"own","sessionId":"native","message":{"content":"한국어 원본 입력"}}),
        serde_json::json!({"type":"user","uuid":"stop","sessionId":"native","message":{"content":[{"type":"text","text":mark}]}}),
    ]
    .map(|row| format!("{row}\n"));
    std::fs::write(&f.ticket.source.as_ref().unwrap().transcript, rows.concat()).unwrap();
    let mut shared = b2b_shared(Some(pool.clone()));
    let (key, actor) = b2b_active(&f, &shared).await;
    b2b_row(&f);
    supply_register(&f, &shared, key, Some(actor)).await;
    barrier_receipt(&f);
    let registrations = f.history.native_classification_registrations().unwrap();
    let (own, end) = (rows[0].len() as u64, rows.concat().len() as u64);
    let hashes = crate::services::tui_input::human_frame::history::v2::native::frame_hashes(mark);
    let stamp = registrations_fingerprint(&registrations).unwrap();
    let barrier: Barrier = serde_json::from_value(serde_json::json!({"namespace":"native-fixture","ticket":f.ticket,"membership_range":[0,own],"membership_row":"own","foreign_range":[own,end],"foreign_row":"stop","foreign_hashes":hashes,"through":end,"registrations_fingerprint":stamp})).unwrap();
    assert!(f.history.record_native_barrier(&barrier).unwrap());
    if boot {
        let inputs = super::super::native_prepare::BootInputs::prepare(&root, 2).unwrap();
        shared = b2b_shared(Some(pool.clone()));
        Arc::get_mut(&mut shared)
            .unwrap()
            .restart
            .current_generation = 2;
        inputs.restore(&shared).await.unwrap();
    }
    let mut capture = SupplyCapture::new(&f, &runtime.path().join("terminal-store"), true);
    capture.poll(&shared).await;
    let saved = f
        .history
        .native_settlement("native-fixture", &f.ticket)
        .unwrap();
    let outcome = saved
        .outcome
        .as_ref()
        .expect("the stored barrier is superseded");
    assert_eq!(outcome.disposition, Disposition::Cancelled);
    let proof = &outcome.extra["native_proof"];
    assert_eq!(proof["supersedes"]["interrupt_row"], "stop");
    assert!(saved.cleanup_complete);
    assert_eq!(saved.barrier("native-fixture").unwrap(), Some(barrier));
    let row = crate::services::discord::inflight::load_inflight_state_read_only(
        &ProviderKind::Claude,
        key.channel_id.get(),
    );
    assert!(row.is_none(), "the result cleans the original row once");
    let mut record = f
        .history
        .native_boot_record("native-fixture", &f.ticket)
        .unwrap();
    let validate = super::super::native_prepare::validate_saved_record;
    validate(&record).unwrap();
    record.settlement.extra["foreign_prompt_barrier"]["foreign_row"] = "other".into();
    let refused = validate(&record).unwrap_err();
    assert_eq!(refused, "native execution crosses foreign prompt barrier");
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn native_8148_terminal_stored_interrupt_barrier_replays_live_and_after_boot_pg() {
    stored_interrupt_barrier_replays(false).await;
    stored_interrupt_barrier_replays(true).await;
}

#[tokio::test]
async fn native_8148_terminal_raw_eof_waits_for_the_execution_end_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let f = Fixture::at(
        runtime.path().join("runtime/injected_frames.json"),
        81480482,
        true,
    );
    let source = f.ticket.source.as_ref().unwrap();
    let own = serde_json::json!({"type":"user","uuid":"own","sessionId":"native","message":{"content":"한국어 원본 입력"}});
    let closer =
        serde_json::json!({"type":"system","subtype":"turn_duration","sessionId":"native"});
    std::fs::write(&source.transcript, format!("{own}\n{closer}")).unwrap();
    let shared = b2b_shared(Some(pool.clone()));
    let (key, actor) = b2b_active(&f, &shared).await;
    b2b_row(&f);
    supply_register(&f, &shared, key, Some(actor)).await;
    barrier_receipt(&f);
    let mut capture = SupplyCapture::new(&f, &runtime.path().join("terminal-store"), true);
    let settled = || {
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
    };
    let row = || {
        let channel = key.channel_id.get();
        crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            channel,
        )
    };
    capture.poll(&shared).await;
    assert_eq!(
        settled().outcome,
        None,
        "no execution end, no raw closer result"
    );
    assert!(row().is_some());
    let nonce = source.target["execution_nonce"].as_str().unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&source.transcript)
        .unwrap();
    file.write_all(b"\n").unwrap();
    capture.poll(&shared).await;
    let saved = settled();
    let outcome = saved
        .outcome
        .as_ref()
        .expect("the completed record settles");
    assert_eq!(outcome.disposition, Disposition::Executed);
    assert!(outcome.extra["native_proof"]["raw"].is_null());
    assert!(saved.cleanup_complete && row().is_none());
    let validate = super::super::native_prepare::validate_saved_record;
    for (execution, accepted) in [(nonce, true), ("another-execution", false)] {
        let mut record = f
            .history
            .native_boot_record("native-fixture", &f.ticket)
            .unwrap();
        let proof = &mut record.settlement.outcome.as_mut().unwrap().extra["native_proof"];
        let range = serde_json::from_value::<[u64; 2]>(proof["closer_range"].clone()).unwrap();
        let raw = witness(&f, range.into(), execution, String::new());
        proof["raw"] = serde_json::to_value(raw).unwrap();
        let checked = validate(&record).map_err(|e| e == "native boot result proof mismatch");
        assert_eq!(
            checked,
            if accepted { Ok(()) } else { Err(true) },
            "{execution}"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn herdr_native_terminal_releases_the_rowless_mailbox_and_wakes_the_owner_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::discord::{
        inflight::{self, GuardedClearOutcome, InflightTurnIdentity},
        turn_completion_events::{TurnCompletionEvent, subscribe_turn_completion_events},
    };

    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let channel = 84570001;
    let target = serde_json::json!({"host":"herdr","owner":"fixture-pane","execution_nonce":NONCE,"root":rig.socket()});
    let f = Fixture::at_target(
        runtime.path().join("runtime/injected_frames.json"),
        channel,
        true,
        target,
    );
    let path = &f.ticket.source.as_ref().unwrap().transcript;
    fs::write(path, format!("{OWN_ROW}\n")).unwrap();
    let shared = b2b_shared(Some(pool.clone()));
    let (key, actor) = b2b_active(&f, &shared).await;
    let row = b2b_row(&f);
    supply_register(&f, &shared, key, Some(actor.clone())).await;
    barrier_receipt(&f);
    let mut events = subscribe_turn_completion_events(&shared);
    let mailbox = shared.mailbox(key.channel_id);
    let settlement = || {
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
    };
    let read_row =
        || inflight::load_inflight_state_read_only_result(&ProviderKind::Claude, channel);
    assert_eq!(
        inflight::clear_inflight_state_for_captured_episode(
            &ProviderKind::Claude,
            channel,
            &InflightTurnIdentity::from_state(&row),
            row.turn_nonce.as_deref(),
        ),
        GuardedClearOutcome::Cleared
    );
    assert!(read_row().unwrap().is_none());
    let mut capture = SupplyCapture::new(&f, &runtime.path().join("store"), true);
    capture.poll(&shared).await;
    assert!(mailbox.has_active_turn().await.unwrap());
    assert!(settlement().outcome.is_none());
    assert_eq!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    );

    writeln!(
        fs::OpenOptions::new().append(true).open(path).unwrap(),
        "{RAW_TAIL}"
    )
    .unwrap();
    capture.poll(&shared).await;
    assert!(
        !mailbox.has_active_turn().await.unwrap(),
        "the native terminal must release its original actor after the projection disappears"
    );
    let settled = settlement();
    assert_eq!(settled.outcome.unwrap().disposition, Disposition::Executed);
    assert!(settled.cleanup_complete && read_row().unwrap().is_none());
    assert_eq!(
        events.try_recv(),
        Ok(TurnCompletionEvent::for_turn(
            key.channel_id,
            key.user_msg_id
        )),
        "the queue owner must receive its existing eligible edge"
    );
    capture.poll(&shared).await;
    assert_eq!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    );
    pool.close().await;
    db.drop().await;
}

/// A raw closer witness for `[through, len)` of the fixture transcript.
fn witness(f: &Fixture, (through, len): (u64, u64), nonce: &str, stamp: String) -> RawWitness {
    let source = f.ticket.source.as_ref().unwrap();
    let (dev, ino, modified) = (
        source.file[0],
        source.file[1],
        std::time::SystemTime::UNIX_EPOCH,
    );
    let fingerprint = Fingerprint {
        len,
        dev,
        ino,
        modified,
        change: (0, 0),
    };
    let tail = Tail {
        through,
        prefix_hash: String::new(),
        fingerprint,
        digest: "0".repeat(64),
    };
    let (execution_nonce, registrations_fingerprint) = (nonce.into(), stamp);
    RawWitness {
        version: 1,
        tail,
        aborted: false,
        execution_nonce,
        registrations_fingerprint,
    }
}

const NONCE: &str = "incarnation-1";
const CASES: [&str; 5] = ["append", "rewrite", "rename", "digest", "stamp"];
const OWN_ROW: &str =
    r#"{"type":"user","uuid":"own","sessionId":"native","message":{"content":"한국어 원본 입력"}}"#;
const RAW_TAIL: &str =
    r#"{"type":"system","subtype":"turn_duration","sessionId":"native","probe":"A"}"#;

async fn ended_fixture(pool: &sqlx::PgPool, shared: &SharedData, rig: &HerdrRig, channel: u64) {
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedLocation, HostedState, test_support,
    };
    use crate::services::session_host::herdr_socket_rig_tests::{
        KEY, NODE, PANE, PROVIDER, SESSION,
    };

    let channel = channel.to_string();
    let mut owner = test_support::owner(&channel);
    owner.discord_token_hash.clone_from(&shared.token_hash);
    owner.logical_key = "fixture-pane".into();
    owner.owner_node = NODE.into();
    let mut hosted = test_support::record(&owner, NONCE, HostedState::Bound);
    hosted.location = Some(HostedLocation {
        host: "herdr".into(),
        execution_node: NODE.into(),
        endpoint_config_key: KEY.into(),
        socket_addr: rig.socket().display().to_string(),
        named_session: SESSION.into(),
        pane_id: PANE.into(),
    });
    hosted.expected = Some(rig.expected(NONCE));
    sqlx::query(
        "INSERT INTO sessions
         (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution)
         VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)",
    )
    .bind(format!("claude/{}/{NODE}:fixture-{channel}", shared.token_hash))
    .bind(&shared.token_hash)
    .bind(&channel)
    .bind(serde_json::to_value(hosted).unwrap())
    .execute(pool)
    .await
    .unwrap();
    rig.show_panes(&[]);
    rig.exists(PROVIDER, false);
}

struct RawCapture {
    port: NativePort,
    origin: NoticeOrigin,
    observed: Observation,
    ticket: Registration,
    tail: Tail,
}

impl RawCapture {
    async fn new(f: &Fixture, shared: &Arc<SharedData>, root: &Path) -> Self {
        let capture = SupplyCapture::new(f, root, true);
        let source = f.ticket.source.as_ref().unwrap();
        let source = SourceId {
            session_id: source.session_id.clone(),
            path: source.transcript.clone(),
            dev: source.file[0],
            ino: source.file[1],
        };
        let binding = SourceBinding {
            channel_id: f.ticket.channel_id,
            provider: ShadowProvider::Claude,
            source: source.clone(),
        };
        let tail = capture.sources.raw_tail(&source).unwrap();
        let origin = NoticeOrigin {
            record: crate::services::tui_o::shadow::SourceRange {
                source,
                start: 0,
                end: tail.through,
            },
            logical_session: "fixture-pane".into(),
            binding_seq: 1,
            execution_nonce: NONCE.into(),
        };
        let port = super::super::native_supply::port(
            f.ticket.channel_id,
            shared.clone(),
            capture.bindings.clone(),
        );
        let snapshot = port.inner.snapshot(origin.clone()).await.unwrap();
        let ticket = snapshot
            .registrations
            .iter()
            .find(|r| r.ticket == f.ticket)
            .unwrap()
            .clone();
        let observed = Observation {
            binding,
            generation: snapshot.generation,
            binding_seq: origin.binding_seq,
            revision: 1,
            through: tail.through,
            registrations_fingerprint: registrations_fingerprint(&snapshot.registrations).unwrap(),
            terminal: None,
            evidence: Vec::new(),
            receipts: Vec::new(),
        };
        Self {
            port,
            origin,
            observed,
            ticket,
            tail,
        }
    }

    async fn prove(&self, tail: Tail) -> Result<RawWitness, String> {
        self.port
            .inner
            .terminal(
                self.observed.clone(),
                self.ticket.clone(),
                tail,
                self.origin.clone(),
            )
            .await
    }

    async fn close(&self, raw: RawWitness) -> Observation {
        let snapshot = self.port.inner.snapshot(self.origin.clone()).await.unwrap();
        let projection = Projection::reopen(
            self.observed.binding.clone(),
            self.tail.through,
            &self.tail.prefix_hash,
        )
        .unwrap()
        .1
        .unwrap();
        let analysis = projection
            .analyze_closed(&snapshot.registrations, Some(&raw))
            .unwrap();
        assert_eq!(analysis.evidence.len(), 1, "one original at this EOF");
        Observation {
            evidence: analysis.evidence,
            terminal: Some(raw),
            ..self.observed.clone()
        }
    }
}

fn replace_tail(path: &Path, through: u64) {
    use std::io::{Seek, SeekFrom};
    let mut file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start(through)).unwrap();
    file.write_all(
        br#"{"type":"system","subtype":"turn_duration","sessionId":"native","probe":"B"}"#,
    )
    .unwrap();
    let end = file.stream_position().unwrap();
    file.set_len(end).unwrap();
    file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(12))
        .unwrap();
}

async fn check_file_change(
    case: &str,
    rig: &HerdrRig,
    f: &Fixture,
    shared: &Arc<SharedData>,
    root: &Path,
) {
    let source = f.ticket.source.as_ref().unwrap();
    let path = &source.transcript;
    let rows = format!("{OWN_ROW}\n{RAW_TAIL}");
    fs::write(path, &rows).unwrap();
    let (key, actor) = b2b_active(f, shared).await;
    b2b_row(f);
    supply_register(f, shared, key, Some(actor)).await;
    barrier_receipt(f);

    let capture = RawCapture::new(f, shared, &root.join(format!("{case}-old"))).await;
    capture.prove(capture.tail.clone()).await.unwrap();
    let mut stale = capture.tail.clone();
    let mut restore = None;
    let error = match case {
        "append" => {
            fs::OpenOptions::new()
                .append(true)
                .open(path)
                .unwrap()
                .write_all(b" ")
                .unwrap();
            capture.prove(stale).await.unwrap_err()
        }
        "rewrite" => {
            replace_tail(path, stale.through);
            capture.prove(stale).await.unwrap_err()
        }
        "rename" => {
            let saved = path.with_extension("saved");
            fs::rename(path, &saved).unwrap();
            fs::write(path, &rows).unwrap();
            restore = Some(saved);
            capture.prove(stale).await.unwrap_err()
        }
        "digest" => {
            replace_tail(path, stale.through);
            stale.fingerprint = Fingerprint::of(&fs::metadata(path).unwrap()).unwrap();
            capture.prove(stale).await.unwrap_err()
        }
        "stamp" => {
            let file = File::open(path).unwrap();
            rig.on_reply(move |call| {
                use crate::services::session_host::herdr_socket_rig_tests::HerdrRequest;
                if matches!(call.request, HerdrRequest::SessionSnapshot { .. }) {
                    file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(13))
                        .unwrap();
                }
            });
            let result = capture.prove(stale).await;
            rig.on_reply(|_| {});
            result.unwrap_err()
        }
        _ => unreachable!("{case}"),
    };
    let expected = match case {
        "append" | "rewrite" => "native tail differs from the capture",
        "rename" => "path names a different inode",
        "digest" => "native tail changed during its read",
        "stamp" => "native tail changed after its execution ended",
        _ => unreachable!("{case}"),
    };
    assert_eq!(
        error, expected,
        "{case}: one rejected stale or unreadable proof"
    );

    let settled = || {
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
    };
    assert!(
        settled().outcome.is_none(),
        "{case}: refusal preserves custody"
    );
    assert!(
        crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            key.channel_id.get(),
        )
        .is_some()
    );
    if let Some(saved) = restore {
        fs::rename(saved, path).unwrap();
    }
    if case == "append" {
        replace_tail(path, capture.tail.through);
    }

    let fresh = RawCapture::new(f, shared, &root.join(format!("{case}-fresh"))).await;
    let raw = fresh
        .prove(fresh.tail.clone())
        .await
        .unwrap_or_else(|error| panic!("{case}: fresh proof: {error}"));
    let closed = fresh.close(raw.clone()).await;
    assert!(
        fresh
            .port
            .inner
            .offer(closed.clone(), closed, 0, fresh.origin.clone())
            .await
            .unwrap()
    );
    let saved = settled();
    let outcome = saved.outcome.unwrap();
    assert_eq!(
        outcome.disposition,
        Disposition::Executed,
        "{case}: one fresh result"
    );
    assert_eq!(
        outcome.extra["native_proof"]["raw"],
        serde_json::to_value(raw).unwrap()
    );
    assert!(saved.cleanup_complete);
}

#[tokio::test]
async fn native_8148_terminal_raw_eof_rejects_changed_file_and_reobserves_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;

    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    for (index, case) in CASES.into_iter().enumerate() {
        let channel = 81480620 + index as u64;
        let target = serde_json::json!({"host":"herdr","owner":"fixture-pane","execution_nonce":NONCE,"root":rig.socket()});
        let path = runtime
            .path()
            .join("runtime")
            .join(format!("{case}-history.json"));
        let f = Fixture::at_target(path, channel, true, target);
        let shared = b2b_shared(Some(pool.clone()));
        ended_fixture(&pool, &shared, &rig, channel).await;
        check_file_change(case, &rig, &f, &shared, runtime.path()).await;
    }
    pool.close().await;
    db.drop().await;
}

type PortReply<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Default)]
struct PollCalls {
    terminals: Vec<Terminal>,
    offers: Vec<Result<bool, String>>,
}

struct PollProbe {
    port: Arc<dyn EvidencePort>,
    append_after_proof: Mutex<Option<PathBuf>>,
    calls: Arc<Mutex<PollCalls>>,
}

impl EvidencePort for PollProbe {
    fn snapshot(&self, origin: NoticeOrigin) -> PortReply<'_, Result<Snapshot, String>> {
        self.port.snapshot(origin)
    }

    fn offer(
        &self,
        observed: Observation,
        current: Observation,
        index: usize,
        origin: NoticeOrigin,
    ) -> PortReply<'_, Result<bool, String>> {
        Box::pin(async move {
            let result = self.port.offer(observed, current, index, origin).await;
            self.calls.lock().unwrap().offers.push(result.clone());
            result
        })
    }

    fn terminal(
        &self,
        observed: Observation,
        ticket: Registration,
        tail: Tail,
        origin: NoticeOrigin,
    ) -> PortReply<'_, Terminal> {
        Box::pin(async move {
            let result = self.port.terminal(observed, ticket, tail, origin).await;
            if result.is_ok()
                && let Some(path) = self.append_after_proof.lock().unwrap().take()
            {
                fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .unwrap()
                    .write_all(b" ")
                    .unwrap();
            }
            self.calls.lock().unwrap().terminals.push(result.clone());
            result
        })
    }

    fn receipt(
        &self,
        observed: Observation,
        current: Observation,
        index: usize,
        origin: NoticeOrigin,
    ) -> PortReply<'_, Result<(), String>> {
        self.port.receipt(observed, current, index, origin)
    }

    fn abstain(
        &self,
        observed: Observation,
        current: Observation,
        proofs: (Vec<Barrier>, Vec<Barrier>),
        idle: bool,
        origin: NoticeOrigin,
    ) -> PortReply<'_, Result<(), String>> {
        self.port.abstain(observed, current, proofs, idle, origin)
    }

    fn retry(&self) -> PortReply<'_, ()> {
        self.port.retry()
    }
}

fn recording_port(
    f: &Fixture,
    shared: &Arc<SharedData>,
    capture: &SupplyCapture,
    append_after_proof: Option<PathBuf>,
) -> (NativePort, Arc<Mutex<PollCalls>>) {
    let port = super::super::native_supply::port(
        f.ticket.channel_id,
        shared.clone(),
        capture.bindings.clone(),
    );
    let calls = Arc::new(Mutex::new(PollCalls::default()));
    let inner = Arc::new(PollProbe {
        port: port.inner,
        append_after_proof: Mutex::new(append_after_proof),
        calls: calls.clone(),
    });
    (NativePort { inner }, calls)
}

enum PollFault {
    AppendAfterProof,
    EofRead,
}

async fn check_writer_rejection(fault: PollFault) {
    use crate::db::auto_queue::test_support::TestPostgresDb;

    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let channel = 81480640;
    let target = serde_json::json!({"host":"herdr","owner":"fixture-pane","execution_nonce":NONCE,"root":rig.socket()});
    let f = Fixture::at_target(
        runtime.path().join("runtime/injected_frames.json"),
        channel,
        true,
        target,
    );
    let path = &f.ticket.source.as_ref().unwrap().transcript;
    fs::write(path, format!("{OWN_ROW}\n{RAW_TAIL}")).unwrap();
    let shared = b2b_shared(Some(pool.clone()));
    ended_fixture(&pool, &shared, &rig, channel).await;
    let (key, actor) = b2b_active(&f, &shared).await;
    b2b_row(&f);
    supply_register(&f, &shared, key, Some(actor)).await;
    barrier_receipt(&f);

    let append_path = match fault {
        PollFault::AppendAfterProof => Some(path.clone()),
        PollFault::EofRead => {
            super::super::native_supply::native_terminal::read_probe::after_next_read(|bytes| {
                assert_eq!(
                    bytes,
                    RAW_TAIL.as_bytes(),
                    "the bounded read got the whole tail"
                );
                Err(io::Error::other("injected EOF read failure"))
            });
            None
        }
    };
    let mut capture = SupplyCapture::new(&f, &runtime.path().join("first-store"), true);
    let (port, calls) = recording_port(&f, &shared, &capture, append_path);
    capture.sources.offer_native(&port).await;
    {
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.terminals.len(),
            1,
            "one actual terminal proof attempt"
        );
        match fault {
            PollFault::AppendAfterProof => {
                assert!(calls.terminals[0].is_ok(), "the proof predates the append");
                assert_eq!(
                    calls.offers,
                    [Err("native observation superseded".into())],
                    "the writer must reject its stale terminal proof"
                );
                replace_tail(path, calls.terminals[0].as_ref().unwrap().tail.through);
            }
            PollFault::EofRead => {
                assert_eq!(
                    calls.terminals[0].as_ref().unwrap_err(),
                    "injected EOF read failure",
                    "the terminal port must propagate the bounded read error"
                );
                assert!(
                    calls.offers.is_empty(),
                    "a failed terminal proof must not be offered"
                );
            }
        }
    }
    let saved = || {
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap()
    };
    let row = || {
        crate::services::discord::inflight::load_inflight_state_read_only(
            &ProviderKind::Claude,
            channel,
        )
    };
    assert!(saved().outcome.is_none() && !saved().cleanup_complete);
    assert!(row().is_some(), "the rejected poll preserves the owned row");

    let mut fresh = SupplyCapture::new(&f, &runtime.path().join("fresh-store"), true);
    let (port, calls) = recording_port(&f, &shared, &fresh, None);
    fresh.sources.offer_native(&port).await;
    let settled = saved();
    let outcome = settled
        .outcome
        .as_ref()
        .expect("a fresh poll settles the same ticket");
    assert_eq!(outcome.disposition, Disposition::Executed);
    assert!(settled.cleanup_complete && row().is_none());
    fresh.sources.offer_native(&port).await;
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.terminals.len(), 1);
        assert_eq!(
            calls.offers,
            [Ok(true)],
            "only one actual result offer succeeds"
        );
        assert_eq!(
            outcome.extra["native_proof"]["raw"],
            serde_json::to_value(calls.terminals[0].as_ref().unwrap()).unwrap()
        );
    }
    assert_eq!(saved(), settled);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn native_8148_terminal_writer_rejects_tail_appended_after_proof_pg() {
    check_writer_rejection(PollFault::AppendAfterProof).await;
}

#[tokio::test]
async fn native_8148_terminal_writer_holds_full_tail_read_failure_pg() {
    check_writer_rejection(PollFault::EofRead).await;
}

#[path = "native_rowless_wait_tests.rs"]
mod rowless_wait;
