//! A kept port is still driven on the polls whose observation the memo answers.
use super::*;
use crate::services::tui_o::writer::actor::native_input::{
    EvidencePort, NativePort, Snapshot, Terminal,
};
use crate::services::tui_o::writer::input_facts::native::{Barrier, Registration, terminal::Tail};
use crate::services::tui_o::writer::input_notices::NoticeOrigin;
use std::sync::atomic::{AtomicUsize, Ordering};

type Held<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The real port, counting its retry, snapshot and Idle abstain calls.
struct Counted {
    inner: Arc<dyn EvidencePort>,
    calls: [AtomicUsize; 3],
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
        self.inner.abstain(observed, current, proofs, idle, origin)
    }
    fn receipt(
        &self,
        observed: Observation,
        current: Observation,
        index: usize,
        origin: NoticeOrigin,
    ) -> Held<'_, Result<(), String>> {
        self.inner.receipt(observed, current, index, origin)
    }
    fn terminal(
        &self,
        observed: Observation,
        ticket: Registration,
        tail: Tail,
        origin: NoticeOrigin,
    ) -> Held<'_, Terminal> {
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

#[tokio::test]
async fn kept_port_still_drives_retries_when_reduce_is_skipped_pg() {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let history = runtime.path().join("runtime/kept-port.json");
    let f = Fixture::at(history, 83850020, true);
    supply_rows(&f, false, true);
    let shared = b2b_shared(Some(pool.clone()));
    let (key, actor) = b2b_active(&f, &shared).await;
    b2b_row(&f);
    let mut capture = SupplyCapture::new(&f, &runtime.path().join("kept-port-store"), true);
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
    let calls = || counted.calls.each_ref().map(|c| c.load(Ordering::SeqCst));
    let saved = || f.history.native_settlement("native-fixture", &f.ticket);
    poll(&mut capture, &port).await;
    let (reduced, before) = (capture.sources.native_reductions(), calls());
    poll(&mut capture, &port).await;
    // Nothing changed, so nothing is reduced again; the port is retried, read for custody and
    // offered the Idle handoff all the same.
    assert_eq!(capture.sources.native_reductions(), reduced);
    let after = calls();
    assert_eq!((after[0], after[2]), (before[0] + 1, before[2] + 1));
    assert!(reduced > 0 && after[1] > before[1]);
    assert!(saved().unwrap().outcome.is_none());
    // Custody arrives with the transcript unchanged: the same port settles it and frees the queue.
    supply_register(&f, &shared, key, Some(actor.clone())).await;
    let mut events =
        crate::services::discord::turn_completion_events::subscribe_turn_completion_events(&shared);
    poll(&mut capture, &port).await;
    let result = saved().unwrap();
    assert_eq!(result.outcome.unwrap().disposition, Disposition::Executed);
    assert!(result.cleanup_complete);
    assert!(events.try_recv().unwrap().queue_is_eligible());
    pool.close().await;
    db.drop().await;
}
