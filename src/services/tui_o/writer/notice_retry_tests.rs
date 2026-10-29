use super::*;
use crate::services::tui_o::repost::admission::OriginalGate;
use crate::services::tui_o::repost::projection::ReceiptOverlay;
use crate::services::tui_o::repost::runner::{RepostHook, Turned};
use crate::services::tui_o::store::ledger::{LocalRetryMark, PieceRecord};
use crate::services::tui_o::writer::local_retry;

fn not_found(harness: &Harness, serial: u64, kind: UnitKind, id: &str, payload: &str) {
    let mut key = unit(id);
    key.kind = kind;
    let mut store = harness.channel();
    store
        .append_ledger(LedgerEntry::Prepared {
            serial,
            unit_key: key,
            piece_index: 0,
            payload: payload.into(),
            anchor_id: 100,
            epoch: 1,
            local_retry: Some(LocalRetryMark::Budget),
            notice_origin: None,
        })
        .unwrap();
    store
        .append_ledger(LedgerEntry::NotFound { serial })
        .unwrap();
    assert!(store.ledger().violation().is_none());
}

struct LocalOnlyHook(Arc<AtomicUsize>);
impl RepostHook<Writer> for LocalOnlyHook {
    fn turn<'a>(&'a mut self, _: &'a mut Writer) -> Turned<'a, ()> {
        Box::pin(async {})
    }
    fn original(&mut self, _: u64, record: PieceRecord) -> Turned<'_, OriginalGate> {
        assert_ne!(record.unit_key.kind, UnitKind::InputNotice);
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { OriginalGate::SendLocal })
    }
    fn receipts(&mut self) -> Turned<'_, ReceiptOverlay> {
        Box::pin(async { ReceiptOverlay::Disabled })
    }
}

#[tokio::test(start_paused = true)]
async fn a_notice_retry_budget_does_not_hold_fresh_body_with_repost_on_or_off() {
    for repost in [false, true] {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let harness = Harness::new();
        harness.gate.acquired();
        not_found(&harness, 0, UnitKind::InputNotice, "notice", "-# ⌨️ held");
        let original = Arc::new(AtomicUsize::new(0));
        let hook = repost
            .then(|| Box::new(LocalOnlyHook(original.clone())) as Box<dyn RepostHook<Writer>>);
        let mut writer = harness.writer().with_repost(hook);
        assert!(
            !writer.has_local_retry(),
            "notice debt must not become the Body retry barrier"
        );
        let body = piece("body", "body behind notice debt");
        let Derived::Piece(body_piece) = &body else {
            panic!("body fixture")
        };
        assert!(local_retry::may_prepare(
            writer.store().ledger(),
            body_piece,
            None
        ));
        assert_eq!(writer.deliver(&body).await, Step::Done);
        assert_eq!(harness.port.posts(), ["body behind notice debt"]);
        assert_eq!(original.load(Ordering::SeqCst), usize::from(repost));
        assert_eq!(
            writer.store().ledger().piece(0).unwrap().outcome,
            Some(PieceOutcome::NotFound)
        );
        assert_eq!(
            writer.store().ledger().piece(1).unwrap().unit_key.kind,
            UnitKind::Body
        );
        let notice = writer.store().ledger().piece(0).unwrap().unit_key.clone();
        assert!(writer.store().ledger().excluded(&notice).is_none());
    }
    let harness = Harness::new();
    harness.gate.acquired();
    not_found(&harness, 0, UnitKind::Body, "older-body", "older body");
    let mut writer = harness.writer();
    assert!(
        writer.has_local_retry(),
        "the existing Body retry barrier remains"
    );
    assert_eq!(
        writer.deliver(&piece("later-body", "later")).await,
        Step::LeaseBusy
    );
    assert!(harness.port.posts().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_withheld_notice_retry_does_not_starve_the_first_body_retry_after_reopening() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let harness = Harness::new();
    harness.gate.acquired();
    not_found(
        &harness,
        0,
        UnitKind::InputNotice,
        "notice",
        "-# ⌨️ missing evidence",
    );
    not_found(&harness, 1, UnitKind::Body, "body", "stored Body retry");
    let when = Utc::now() + chrono::Duration::days(1);
    let mut writer = harness.writer();
    assert!(writer.has_local_retry());
    assert_eq!(writer.retry_local(when).await, Step::Done);
    assert_eq!(
        harness.port.posts(),
        ["stored Body retry"],
        "the missing notice origin must not block a different retry"
    );
    let ledger = writer.store().ledger();
    let body = ledger.piece(2).expect("Body retry is durably prepared");
    assert_eq!(body.unit_key.kind, UnitKind::Body);
    assert_eq!(
        body.local_retry,
        Some(LocalRetryMark::Attempt { root_serial: 1 })
    );
    assert_eq!(
        ledger.piece(0).unwrap().outcome,
        Some(PieceOutcome::NotFound)
    );
    assert_eq!(
        ledger
            .latest_piece(&ledger.piece(0).unwrap().unit_key, 0)
            .unwrap()
            .0,
        0
    );
    drop(writer);
    let mut reopened = harness.writer();
    assert!(!reopened.has_local_retry());
    let _ = reopened.retry_local(when).await;
    assert_eq!(
        harness.port.posts(),
        ["stored Body retry"],
        "one Body retry across restarts"
    );
    assert_eq!(reopened.store().ledger().next_serial(), 3);
}
