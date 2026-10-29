use super::*;
use crate::services::tui_o::repost::admission::OriginalGate;
use crate::services::tui_o::repost::projection::ReceiptOverlay;
use crate::services::tui_o::repost::runner::{RepostHook, Turned};
use crate::services::tui_o::store::ledger::PieceRecord;

struct CountedRepost(Arc<std::sync::atomic::AtomicUsize>);

impl RepostHook<Writer> for CountedRepost {
    fn turn<'a>(&'a mut self, _: &'a mut Writer) -> Turned<'a, ()> {
        Box::pin(async {})
    }

    fn original(&mut self, _: u64, record: PieceRecord) -> Turned<'_, OriginalGate> {
        assert_ne!(record.unit_key.kind, UnitKind::InputNotice);
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { OriginalGate::SendLocal })
    }

    fn receipts(&mut self) -> Turned<'_, ReceiptOverlay> {
        Box::pin(async { ReceiptOverlay::default() })
    }
}

fn input(uuid: &str) -> Vec<u8> {
    let mut line = serde_json::to_vec(&serde_json::json!({"type":"user","uuid":uuid,
        "message":{"role":"user","content":"same native input"}}))
    .unwrap();
    line.push(b'\n');
    line
}

#[tokio::test(start_paused = true)]
async fn disabled_notice_composition_captures_restarts_and_keeps_body_transport_live() {
    for repost in [false, true] {
        let (harness, path, source) = switched_over(b"");
        harness.gate.acquired();
        let admissions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        append(&path, &[input("one"), input("two")].concat());
        for restart in 0..2 {
            let hook = repost.then(|| {
                Box::new(CountedRepost(admissions.clone())) as Box<dyn RepostHook<Writer>>
            });
            let (stop, task) = spawn(harness.writer().with_repost(hook));
            polls(3).await;
            assert_eq!(
                harness.channel().cursor(&source).unwrap().captured_through,
                std::fs::metadata(&path).unwrap().len()
            );
            assert!(harness.port.posts().is_empty());
            assert_eq!(harness.channel().ledger().next_serial(), 0);
            assert_eq!(std::fs::metadata(ledger_path(&harness)).unwrap().len(), 0);
            assert_eq!(
                harness.channel().ledger().input_notices(),
                &Default::default()
            );
            assert!(
                !harness
                    ._runtime
                    .path()
                    .join("o_store/input_notice.floor")
                    .exists()
            );
            assert!(harness.channel().ledger().gc_segments(&source).is_empty());
            assert_eq!(admissions.load(std::sync::atomic::Ordering::SeqCst), 0);
            if restart == 1 {
                append(&path, &row("control", "body transport control"));
                polls(3).await;
                assert_eq!(harness.port.posts(), ["body transport control"]);
                assert_eq!(
                    admissions.load(std::sync::atomic::Ordering::SeqCst),
                    usize::from(repost)
                );
            }
            assert!(harness.alarms.taken().is_empty());
            halt(stop, task).await;
        }
    }
}
