use super::*;
use crate::services::tui_o::writer::actor::native_input::{EvidencePort, NativePort, Snapshot};
use crate::services::tui_o::writer::input_facts::native::Observation;
use crate::services::tui_o::writer::input_notices::NoticeOrigin;
use std::pin::Pin;

#[tokio::test(start_paused = true)]
async fn known_command_receipt_excludes_logical_notice_across_restart() {
    let c = Case::new("known-command-restarts");
    let frame = c.frame();
    let (registry, ticket) = c.v2(&frame, 8822, false);
    append_queued_command(&c, &frame);
    assert_eq!(
        registry.receipt(
            NAMESPACE,
            &ticket,
            (&c.path, 0, "command", Some("physical-command"), &frame)
        ),
        Some(true)
    );

    let mut actor = c.open();
    c.run(&mut actor).await;
    assert_eq!(c.notices(), 0);

    append(&c.path, &row("original-input", &frame));
    append(&c.path, &row("independent-input", &frame));
    actor.read_sources_for_test();
    drop(actor);

    let mut restarted = c.open();
    c.run(&mut restarted).await;
    let ledger = restarted.writer_mut_for_test().store().ledger();
    assert_eq!(
        ledger.excluded(&input_key("original-input")),
        Some("input_notice_source")
    );
    let (_, independent) = ledger
        .latest_piece(&input_key("independent-input"), 0)
        .expect("a different input with the same text owes its own notice");
    assert!(matches!(
        independent.outcome,
        Some(crate::services::tui_o::store::ledger::PieceOutcome::Posted(
            _
        ))
    ));
    assert_eq!(c.notices(), 1);
}

type PortFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

struct ReceiptPort {
    registry: history::v2::Registry,
    accepted: Mutex<Vec<(usize, String)>>,
}

impl EvidencePort for ReceiptPort {
    fn snapshot(&self, _: NoticeOrigin) -> PortFuture<'_, Result<Snapshot, String>> {
        Box::pin(async {
            Ok(Snapshot {
                generation: 1,
                registrations: self.registry.native_classification_registrations()?,
                registry_revisions: Vec::new(),
                has_open_barrier: false,
            })
        })
    }

    fn offer(
        &self,
        _: Observation,
        _: Observation,
        _: usize,
        _: NoticeOrigin,
    ) -> PortFuture<'_, Result<bool, String>> {
        Box::pin(async { Err("only membership receipts are expected".into()) })
    }

    fn receipt(
        &self,
        observed: Observation,
        current: Observation,
        index: usize,
        _: NoticeOrigin,
    ) -> PortFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let receipt = &observed.receipts[index];
            assert_eq!(Some(receipt), current.receipts.get(index));
            let (namespace, ticket) = receipt.identity();
            let (kind, row, range, hash) = receipt.record();
            self.registry.try_native_receipt(
                namespace,
                ticket,
                (kind, row, range, current.through, hash),
                &current.registrations_fingerprint,
            )?;
            self.accepted
                .lock()
                .unwrap()
                .push((observed.receipts.len(), row.into()));
            Ok(())
        })
    }

    fn retry(&self) -> PortFuture<'_, ()> {
        Box::pin(async {})
    }
}

#[tokio::test(start_paused = true)]
async fn known_command_receipts_exclude_before_second_native_offer() {
    let c = Case::new("known-command-batch");
    let frame_a = c.frame();
    let frame_b = format!("{frame_a}\n두 번째 입력");
    let registry = history::v2::Registry::runtime().unwrap();
    for (id, physical, input, frame) in [
        (8823, "command-a", "input-a", &frame_a),
        (8824, "command-b", "input-b", &frame_b),
    ] {
        let key = registration(&c.path, &c.logical, OWNER_NONCE, id, false);
        registry
            .prepare(NAMESPACE, &registered_input(&key, frame))
            .unwrap();
        registry
            .prepare_native_intent(
                NAMESPACE,
                &key,
                CHANNEL,
                &history::v2::native::Intent::Follower,
            )
            .unwrap();
        let ticket = registry.native_ticket(NAMESPACE, &key).unwrap();
        registry.note_native_any_key(NAMESPACE, &ticket).unwrap();
        let offset = std::fs::metadata(&c.path).unwrap().len();
        append_command(&c, physical, input, frame);
        assert_eq!(
            registry.receipt(
                NAMESPACE,
                &key,
                (&c.path, offset, "command", Some(physical), frame)
            ),
            Some(true)
        );
    }

    let (writer, bindings) = c.open().into_writer_and_bindings_for_test();
    let mut actor = Actor::new_for_test(
        writer,
        UnitDeriver::new(CHANNEL, ShadowProvider::Claude)
            .with_input_notices(Projection::for_tests()),
        Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone()).with_native_inputs(true),
        bindings,
    );
    resume(&mut actor);
    c.run(&mut actor).await;
    assert_eq!(c.notices(), 0);

    let port = Arc::new(ReceiptPort {
        registry,
        accepted: Mutex::new(Vec::new()),
    });
    let native = NativePort {
        inner: port.clone(),
    };
    append(&c.path, &row("input-a", &frame_a));
    append(&c.path, &row("input-b", &frame_b));
    let fresh = actor.read_sources_for_test();
    actor.sources_mut_for_test().offer_native(&native).await;
    assert_eq!(*port.accepted.lock().unwrap(), [(2, "input-a".to_owned())]);

    actor.prepare_notices_for_test(&fresh).await;
    actor
        .deliver_owed_for_test(&crate::services::tui_o::writer::progress::Publisher::new(
            CHANNEL,
        ))
        .await;
    assert_eq!(
        c.notices(),
        0,
        "the first drain precedes the second native receipt"
    );
    c.excluded(&mut actor, "input-a");
    c.excluded(&mut actor, "input-b");
}
