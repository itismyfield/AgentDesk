//! A kept port is still driven on the polls whose observation the memo answers.
use super::*;
use crate::services::tui_o::writer::actor::native_input::{
    EvidencePort, NativePort, Snapshot, Terminal,
};
use crate::services::tui_o::writer::input_facts::native::{Barrier, Registration, terminal::Tail};
use crate::services::tui_o::writer::input_notices::NoticeOrigin;
use std::sync::atomic::{AtomicUsize, Ordering};

type Held<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The real port, counting its retry, snapshot and Idle abstain calls and what it is handed.
struct Counted {
    inner: Arc<dyn EvidencePort>,
    calls: [AtomicUsize; 4],
}
impl EvidencePort for Counted {
    fn snapshot(&self, origin: NoticeOrigin) -> Held<'_, Result<Snapshot, String>> {
        self.calls[1].fetch_add(1, Ordering::SeqCst);
        self.inner.snapshot(origin)
    }
    fn offer(
        &self,
        observed: Observation,
        current: Observation,
        index: usize,
        origin: NoticeOrigin,
    ) -> Held<'_, Result<bool, String>> {
        self.calls[3].fetch_add(1, Ordering::SeqCst);
        self.inner.offer(observed, current, index, origin)
    }
    fn abstain(
        &self,
        observed: Observation,
        current: Observation,
        proofs: (Vec<Barrier>, Vec<Barrier>),
        idle: bool,
        origin: NoticeOrigin,
    ) -> Held<'_, Result<(), String>> {
        self.calls[2].fetch_add(usize::from(idle), Ordering::SeqCst);
        self.calls[3].fetch_add(1, Ordering::SeqCst);
        self.inner.abstain(observed, current, proofs, idle, origin)
    }
    fn receipt(
        &self,
        observed: Observation,
        current: Observation,
        index: usize,
        origin: NoticeOrigin,
    ) -> Held<'_, Result<(), String>> {
        self.calls[3].fetch_add(1, Ordering::SeqCst);
        self.inner.receipt(observed, current, index, origin)
    }
    fn terminal(
        &self,
        observed: Observation,
        ticket: Registration,
        tail: Tail,
        origin: NoticeOrigin,
    ) -> Held<'_, Terminal> {
        self.calls[3].fetch_add(1, Ordering::SeqCst);
        self.inner.terminal(observed, ticket, tail, origin)
    }
    fn retry(&self) -> Held<'_, ()> {
        self.calls[0].fetch_add(1, Ordering::SeqCst);
        self.inner.retry()
    }
}

async fn poll(capture: &mut SupplyCapture, port: &NativePort) {
    capture.sources.follow(&mut capture.writer).unwrap();
    capture
        .sources
        .capture(&mut capture.writer, &mut capture.deriver, &mut capture.owed)
        .unwrap();
    capture.sources.tend(&mut capture.writer).unwrap();
    capture.sources.offer_native(port).await;
}

/// A registered native turn without custody yet, its capture and the counted real port.
struct Rig {
    runtime: crate::config::TestRuntimeRootGuard,
    db: crate::db::auto_queue::test_support::TestPostgresDb,
    pool: sqlx::PgPool,
    f: Fixture,
    shared: Arc<SharedData>,
    turn: (TurnKey, Arc<CancelToken>),
    capture: SupplyCapture,
    counted: Arc<Counted>,
    port: NativePort,
}
impl Rig {
    /// A withdrawn input's notice needs the pool, so its one connection lets a test starve it.
    async fn new(channel: u64, withdraw: bool) -> Self {
        let runtime = crate::config::TestRuntimeRootGuard::new();
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = match withdraw {
            true => db.connect_and_migrate_with_max_connections(1).await,
            false => db.connect_and_migrate().await,
        };
        let f = Fixture::at(runtime.path().join("runtime/kept-port.json"), channel, true);
        supply_rows(&f, withdraw, true);
        let shared = b2b_shared(Some(pool.clone()));
        let turn = b2b_active(&f, &shared).await;
        b2b_row(&f);
        let capture = SupplyCapture::new(&f, &runtime.path().join("kept-port-store"), true);
        let real = super::super::native_supply::port(
            f.ticket.channel_id,
            shared.clone(),
            capture.bindings.clone(),
        );
        let counted = Arc::new(Counted {
            inner: real.inner,
            calls: Default::default(),
        });
        let port = NativePort {
            inner: counted.clone(),
        };
        Self {
            runtime,
            db,
            pool,
            f,
            shared,
            turn,
            capture,
            counted,
            port,
        }
    }
    fn calls(&self) -> [usize; 4] {
        self.counted
            .calls
            .each_ref()
            .map(|c| c.load(Ordering::SeqCst))
    }
    fn saved(&self) -> crate::services::tui_input::human_frame::history::v2::native::Settlement {
        let (history, ticket) = (&self.f.history, &self.f.ticket);
        history.native_settlement("native-fixture", ticket).unwrap()
    }
    fn receipts(&self) -> Vec<String> {
        let mut seen = (self.f.history.native_classification_registrations()).unwrap();
        seen.remove(0).receipts
    }
    async fn custody(&self) {
        let (key, actor) = self.turn.clone();
        supply_register(&self.f, &self.shared, key, Some(actor)).await;
    }
    async fn close(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

use crate::services::discord::turn_completion_events::subscribe_turn_completion_events as completions;

#[tokio::test]
async fn kept_port_still_drives_retries_when_reduce_is_skipped_pg() {
    let mut rig = Rig::new(83850020, false).await;
    poll(&mut rig.capture, &rig.port).await;
    let (reduced, before) = (rig.capture.sources.native_reductions(), rig.calls());
    poll(&mut rig.capture, &rig.port).await;
    // Nothing changed, so nothing is reduced again; the port is retried, read for custody and
    // offered the Idle handoff all the same.
    assert_eq!(rig.capture.sources.native_reductions(), reduced);
    let after = rig.calls();
    assert_eq!((after[0], after[2]), (before[0] + 1, before[2] + 1));
    assert!(reduced > 0 && after[1] > before[1]);
    assert!(rig.saved().outcome.is_none());
    // Custody arrives with the transcript unchanged: the same port settles it and frees the queue.
    rig.custody().await;
    let mut events = completions(&rig.shared);
    poll(&mut rig.capture, &rig.port).await;
    let result = rig.saved();
    assert_eq!(result.outcome.unwrap().disposition, Disposition::Executed);
    assert!(result.cleanup_complete);
    assert!(events.try_recv().unwrap().queue_is_eligible());
    rig.close().await;
}

#[tokio::test]
async fn a_cleanup_left_by_a_failed_submit_is_finished_by_the_kept_ports_retry_pg() {
    let mut rig = Rig::new(83850022, true).await;
    rig.custody().await;
    let mut events = completions(&rig.shared);
    // The only connection is busy, so the notice cannot be queued: the result is saved and its
    // cleanup stays owed.
    let held = rig.pool.acquire().await.unwrap();
    poll(&mut rig.capture, &rig.port).await;
    let owed = rig.saved();
    assert!(owed.outcome.is_some() && !owed.cleanup_complete && events.try_recv().is_err());
    drop(held);
    let retried = rig.calls()[0];
    poll(&mut rig.capture, &rig.port).await;
    let done = rig.saved();
    assert_eq!(
        (rig.calls()[0], &done.outcome),
        (retried + 1, &owed.outcome)
    );
    assert!(done.cleanup_complete && events.try_recv().unwrap().queue_is_eligible());
    rig.close().await;
}

#[tokio::test]
async fn a_cut_source_submits_nothing_until_a_reopened_seed_reads_new_rows_pg() {
    use sha2::Digest;
    let mut rig = Rig::new(83850021, false).await;
    // Without custody the first receipt is offered and held, so every poll would offer it again.
    poll(&mut rig.capture, &rig.port).await;
    assert!(rig.calls()[3] > 0 && rig.receipts().is_empty());
    rig.custody().await;
    // Cut the same inode inside the row the receipt names; the registration offset is 0.
    let path = rig.f.ticket.source.as_ref().unwrap().transcript.clone();
    let bytes = std::fs::read(&path).unwrap();
    let ends = bytes.iter().enumerate().filter(|(_, b)| **b == b'\n');
    let kept = ends.map(|(at, _)| at + 1).nth(1).unwrap();
    let file = std::fs::File::options().append(true).open(&path).unwrap();
    file.set_len(kept as u64).unwrap();
    let (before, capture) = (rig.calls(), &mut rig.capture);
    capture.sources.follow(&mut capture.writer).unwrap();
    let read =
        (capture.sources).capture(&mut capture.writer, &mut capture.deriver, &mut capture.owed);
    assert!(read.is_err(), "the cut halts the channel");
    capture.sources.offer_native(&rig.port).await;
    assert_eq!(
        rig.calls()[3],
        before[3],
        "nothing of the cut prefix is offered"
    );
    assert!(rig.receipts().is_empty() && rig.saved().outcome.is_none());
    // Opened again at the surviving prefix, the source is a new seed and receipts its new row.
    let user = serde_json::json!({"type":"user","uuid":"again","sessionId":"native","message":{"content":"한국어 원본 입력"}});
    let end = serde_json::json!({"type":"system","subtype":"turn_duration"});
    std::io::Write::write_all(&mut &file, format!("{user}\n{end}\n").as_bytes()).unwrap();
    let prefix = hex::encode(sha2::Sha256::digest(&bytes[..kept]));
    let store = rig.runtime.path().join("reopened-store");
    let mut reopened = SupplyCapture::resume_at(&rig.f, &store, true, kept as u64, prefix);
    reopened.poll(&rig.shared).await;
    assert_eq!(rig.receipts(), ["again"]);
    assert_eq!(
        rig.saved().outcome.unwrap().disposition,
        Disposition::Executed
    );
    rig.close().await;
}

#[derive(Default)]
struct ReceiptProbeState {
    registrations: Vec<Registration>,
    receipt_nonces: Vec<String>,
    offered: Vec<Disposition>,
}

struct ReceiptHeadProbe(std::sync::Mutex<ReceiptProbeState>);

impl EvidencePort for ReceiptHeadProbe {
    fn snapshot(&self, _: NoticeOrigin) -> Held<'_, Result<Snapshot, String>> {
        let registrations = self.0.lock().unwrap().registrations.clone();
        Box::pin(async move {
            Ok(Snapshot {
                generation: 2,
                registrations,
            })
        })
    }

    fn receipt(
        &self,
        observed: Observation,
        current: Observation,
        index: usize,
        origin: NoticeOrigin,
    ) -> Held<'_, Result<(), String>> {
        assert_eq!(observed, current);
        let mut state = self.0.lock().unwrap();
        state.receipt_nonces.push(origin.execution_nonce.clone());
        let result = if origin.execution_nonce == "incarnation-1" {
            Err("original device no longer stats as registered".into())
        } else {
            assert_eq!(origin.execution_nonce, "incarnation-2");
            let receipt = &current.receipts[index];
            let (namespace, ticket) = receipt.identity();
            let (_, row, _, _) = receipt.record();
            let registration = state
                .registrations
                .iter_mut()
                .find(|r| r.namespace == namespace && r.ticket == *ticket)
                .unwrap();
            assert_eq!(row, "successor-row");
            registration.receipts.push(row.into());
            Ok(())
        };
        Box::pin(async move { result })
    }

    fn offer(
        &self,
        observed: Observation,
        current: Observation,
        index: usize,
        origin: NoticeOrigin,
    ) -> Held<'_, Result<bool, String>> {
        assert_eq!(observed, current);
        assert_eq!(origin.execution_nonce, "incarnation-2");
        let evidence = &current.evidence[index];
        assert_eq!(evidence.membership_row.as_deref(), Some("successor-row"));
        self.0
            .lock()
            .unwrap()
            .offered
            .push(evidence.disposition.clone());
        Box::pin(async { Ok(true) })
    }

    fn abstain(
        &self,
        _: Observation,
        _: Observation,
        _: (Vec<Barrier>, Vec<Barrier>),
        _: bool,
        _: NoticeOrigin,
    ) -> Held<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn terminal(
        &self,
        _: Observation,
        _: Registration,
        _: Tail,
        _: NoticeOrigin,
    ) -> Held<'_, Terminal> {
        Box::pin(async { Err("probe transcript has complete records".into()) })
    }

    fn retry(&self) -> Held<'_, ()> {
        Box::pin(async {})
    }
}

#[tokio::test]
async fn probe_old_device_head_receipt_and_successor_same_source() {
    use crate::services::tui_o::writer::binding::{BindingCause, BindingRecord, BindingTarget};
    use serde_json::json;
    use sha2::Digest;

    const REFRESH_MOVES_CURSOR: bool = true;
    let root = crate::config::TestRuntimeRootGuard::new();
    let mut f = Fixture::at(
        root.path().join("runtime/receipt-probe.json"),
        830_187,
        true,
    );
    let source = f.ticket.source.clone().unwrap();
    let live = SourceId {
        session_id: source.session_id.clone(),
        path: source.transcript.clone(),
        dev: source.file[0],
        ino: source.file[1],
    };
    let stored = SourceId {
        dev: live.dev ^ (1 << 40),
        ..live.clone()
    };
    let original_rows = [
        json!({"type": "user", "uuid": "original-row", "sessionId": source.session_id, "message": {"content": f.input["payload"]["frame"]}}),
        json!({"type": "system", "subtype": "turn_duration"}),
    ];
    let mut transcript = original_rows
        .iter()
        .map(|row| format!("{row}\n"))
        .collect::<String>();
    let offset = transcript.len() as u64;
    let successor_rows = [
        json!({"type": "user", "uuid": "successor-row", "sessionId": source.session_id, "message": {"content": "successor fixture"}}),
        json!({"type": "system", "subtype": "turn_duration"}),
    ];
    transcript.extend(successor_rows.iter().map(|row| format!("{row}\n")));
    std::fs::write(&live.path, &transcript).unwrap();

    let namespace = "native-fixture";
    let mut successor = f.input.clone();
    successor["identity"]["primary"] = (f.ticket.channel_id + 2).into();
    successor["identity"]["sources"][0]["message_id"] = (f.ticket.channel_id + 2).into();
    successor["attempt"]["nonce"] = "successor-attempt".into();
    successor["attempt"]["target"]["execution_nonce"] = "incarnation-2".into();
    successor["attempt"]["source"]["offset"] = offset.into();
    successor["payload"]["frame"] = "successor fixture".into();
    f.history.prepare(namespace, &successor).unwrap();
    let key = json!({"identity": successor["identity"], "attempt": successor["attempt"]});
    let successor_ticket = crate::services::tui_input::human_frame::history::v2::native::Ticket {
        source: Some(f.history.source(namespace, &key).unwrap()),
        key,
        owning_episode: None,
        ..f.ticket.clone()
    };
    f.history
        .prepare_native(namespace, &successor_ticket)
        .unwrap();

    let mut registrations = f.history.native_classification_registrations().unwrap();
    assert_eq!(registrations.len(), 2);
    let original = registrations
        .iter_mut()
        .find(|r| r.ticket == f.ticket)
        .unwrap();
    original.ticket.source.as_mut().unwrap().file[0] = stored.dev;
    original.ticket.key["attempt"]["source"]["file"][0] = stored.dev.into();
    f.ticket.source.as_mut().unwrap().file[0] = stored.dev;
    let mut capture = SupplyCapture::resume_at(
        &f,
        &root.path().join("receipt-probe-store"),
        true,
        transcript.len() as u64,
        hex::encode(sha2::Sha256::digest(transcript.as_bytes())),
    );
    {
        let mut events = capture.bindings.events.lock().unwrap();
        let mut resume = events[0].clone();
        resume.seq = 2;
        resume.execution_nonce = "incarnation-2".into();
        if let BindingRecord::Bound {
            old, new, cause, ..
        } = &mut resume.record
        {
            *old = Some(stored.clone());
            *new = BindingTarget::Source(live.clone());
            *cause = BindingCause::Resume;
        }
        let mut refresh = resume.clone();
        refresh.seq = 3;
        if let BindingRecord::Bound {
            old,
            cause,
            evidence,
            ..
        } = &mut refresh.record
        {
            *old = Some(live.clone());
            *cause = BindingCause::SameSessionVerified;
            evidence.hook_event = "user_prompt_submit".into();
        }
        events.extend([resume, refresh]);
        capture.bindings.notice.send_replace(3);
    }

    let probe = Arc::new(ReceiptHeadProbe(std::sync::Mutex::new(ReceiptProbeState {
        registrations,
        ..Default::default()
    })));
    let port = NativePort {
        inner: probe.clone(),
    };
    for _ in 0..2 {
        poll(&mut capture, &port).await;
    }
    assert_eq!(
        capture.writer.store().binding_checkpoint().unwrap(),
        Some(3)
    );
    let expected_cursor = if REFRESH_MOVES_CURSOR { &live } else { &stored };
    assert!(capture.writer.store().cursor(expected_cursor).is_some());
    {
        let mut state = probe.0.lock().unwrap();
        assert!(
            state.offered.is_empty(),
            "the original blocks the first candidate slot"
        );
        if REFRESH_MOVES_CURSOR {
            assert!(
                state.receipt_nonces.is_empty(),
                "the old nonce has no binding on the current device"
            );
        } else {
            assert_eq!(state.receipt_nonces, ["incarnation-1", "incarnation-1"]);
        }
        state.registrations.retain(|r| r.ticket == successor_ticket);
        assert_eq!(state.registrations.len(), 1);
        state.receipt_nonces.clear();
    }
    poll(&mut capture, &port).await;
    let state = probe.0.lock().unwrap();
    if REFRESH_MOVES_CURSOR {
        assert_eq!(state.receipt_nonces, ["incarnation-2"]);
        assert_eq!(state.offered, [Disposition::Executed]);
    } else {
        assert!(state.receipt_nonces.is_empty());
        assert!(
            state.offered.is_empty(),
            "the stored device still filters the successor"
        );
    }
}
