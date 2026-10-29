use super::*;
use crate::services::tui_o::store::ledger::LocalRetryMark;
use crate::services::tui_o::writer::local_retry::{self, LOCAL_RETRY_DELAY};

fn prepared(serial: u64, mark: Option<LocalRetryMark>) -> LedgerEntry {
    LedgerEntry::Prepared {
        notice_origin: None,
        serial,
        unit_key: unit("retry"),
        piece_index: 0,
        payload: "원장 본문 ```x```".into(),
        anchor_id: 100,
        epoch: 1,
        local_retry: mark,
    }
}

fn due(writer: &mut Writer) -> chrono::DateTime<Utc> {
    local_retry::pending(writer.store().ledger()).unwrap().due
}

fn old_not_found(harness: &Harness, mark: Option<LocalRetryMark>) {
    let mut store = harness.channel();
    store.append_ledger(prepared(0, mark)).unwrap();
    store
        .append_ledger(LedgerEntry::NotFound { serial: 0 })
        .unwrap();
}

#[test]
fn local_retry_old_rows_and_terminal_times_replay_without_inventing_authority() {
    let old = serde_json::to_value(prepared(0, None)).unwrap();
    assert!(old.get("local_retry").is_none());
    let decoded: LedgerEntry = serde_json::from_value(old).unwrap();
    assert_eq!(decoded, prepared(0, None));
    for marked in [false, true] {
        let harness = Harness::new();
        old_not_found(&harness, marked.then_some(LocalRetryMark::Budget));
        let mut first = harness.writer();
        let at = first.store().ledger().piece(0).unwrap().outcome_at.unwrap();
        drop(first);
        let mut reopened = harness.writer();
        let root = local_retry::pending(reopened.store().ledger());
        assert_eq!(root.is_some(), marked);
        if let Some(root) = root {
            assert_eq!(root.piece.payload, "원장 본문 ```x```");
            assert_eq!(
                root.due,
                at + chrono::Duration::from_std(LOCAL_RETRY_DELAY).unwrap()
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn local_retry_real_deliver_consumes_once_and_children_never_receive_a_budget() {
    for child in [Reply::Created, Reply::Unsent, Reply::CreatedUnseen] {
        let harness = Harness::new();
        harness.gate.acquired();
        harness.port.say(BOT + 1, "readable");
        harness
            .port
            .replies
            .lock()
            .unwrap()
            .extend([Reply::Unsent, child]);
        let item = piece("retry", "원장 본문 ```x```");
        let mut writer = harness.writer();
        assert_eq!(writer.deliver(&item).await, Step::Done);
        assert_eq!(
            writer.store().ledger().piece(0).unwrap().local_retry,
            Some(LocalRetryMark::Budget)
        );
        let when = due(&mut writer);
        assert_eq!(
            writer
                .retry_local(when - chrono::Duration::nanoseconds(1))
                .await,
            Step::LeaseBusy
        );
        assert_eq!(harness.port.posts().len(), 1);
        assert_eq!(
            writer.deliver(&piece("later", "later")).await,
            Step::LeaseBusy
        );
        assert_eq!(writer.retry_local(when).await, Step::Done);
        let expected = match child {
            Reply::Created => PieceOutcome::Posted(1001),
            Reply::Unsent => PieceOutcome::NotFound,
            _ => PieceOutcome::Ambiguous(vec![1001]),
        };
        assert_eq!(
            writer.store().ledger().piece(1).unwrap().outcome,
            Some(expected)
        );
        for _ in 0..3 {
            drop(writer);
            writer = harness.writer();
            assert_eq!(
                writer.retry_local(when + chrono::Duration::days(1)).await,
                Step::Done
            );
            assert_eq!(writer.deliver(&item).await, Step::Done);
            assert_eq!(
                harness.port.posts().len(),
                2,
                "only one additional POST across restarts"
            );
            assert_eq!(writer.store().ledger().next_serial(), 2);
            assert!(!writer.has_local_retry());
        }
        assert_eq!(
            harness.port.posts(),
            ["원장 본문 ```x```", "원장 본문 ```x```"]
        );
    }
}

#[tokio::test(start_paused = true)]
async fn local_retry_gate_lease_and_stop_hold_before_prepared_and_resume_on_restart() {
    let harness = Harness::new();
    harness.gate.acquired();
    old_not_found(&harness, Some(LocalRetryMark::Budget));
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let mut writer = harness.writer().with_repost_stop(stopped);
    let when = due(&mut writer);
    harness.lease.busy.store(true, Ordering::SeqCst);
    assert_eq!(writer.retry_local(when).await, Step::LeaseBusy);
    harness.lease.busy.store(false, Ordering::SeqCst);
    let gate = Arc::clone(&harness.gate);
    *harness.lease.on_acquire.lock().unwrap() = Some(Box::new(move || gate.uncertain()));
    assert_eq!(writer.retry_local(when).await, Step::NoGateway);
    *harness.lease.on_acquire.lock().unwrap() = None;
    harness.gate.acquired();
    let stop_on_lease = stop.clone();
    *harness.lease.on_acquire.lock().unwrap() = Some(Box::new(move || {
        stop_on_lease.send_replace(true);
    }));
    assert_eq!(writer.retry_local(when).await, Step::LeaseBusy);
    assert_eq!(writer.store().ledger().next_serial(), 1);
    assert!(harness.port.posts().is_empty());
    *harness.lease.on_acquire.lock().unwrap() = None;
    drop(writer);
    let mut restarted = harness.writer();
    assert_eq!(restarted.retry_local(when).await, Step::Done);
    assert_eq!(harness.port.posts(), ["원장 본문 ```x```"]);
    assert!(!restarted.has_local_retry());
}

#[tokio::test(start_paused = true)]
async fn local_retry_open_child_and_old_not_found_cannot_create_another_post() {
    for mark in [None, Some(LocalRetryMark::Budget)] {
        let harness = Harness::new();
        harness.gate.acquired();
        harness.port.say(BOT + 1, "readable");
        old_not_found(&harness, mark.clone());
        if mark.is_some() {
            harness
                .channel()
                .append_ledger(prepared(
                    1,
                    Some(LocalRetryMark::Attempt { root_serial: 0 }),
                ))
                .unwrap();
        }
        for _ in 0..3 {
            let mut writer = harness.writer();
            assert_eq!(
                writer
                    .retry_local(Utc::now() + chrono::Duration::days(1))
                    .await,
                Step::Done
            );
            assert!(!writer.has_local_retry());
            assert!(harness.port.posts().is_empty());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn local_retry_attempt_is_durable_before_the_child_request_is_created() {
    let harness = Harness::new();
    harness.gate.acquired();
    old_not_found(&harness, Some(LocalRetryMark::Budget));
    let path = ledger_path(&harness);
    *harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
        let text = std::fs::read_to_string(&path).unwrap();
        let line: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(line["entry"]["type"], "prepared");
        assert_eq!(line["entry"]["serial"], 1);
        assert_eq!(
            line["entry"]["local_retry"],
            serde_json::json!({"kind":"attempt","root_serial":0})
        );
    }));
    let mut writer = harness.writer();
    let when = due(&mut writer);
    assert_eq!(writer.retry_local(when).await, Step::Done);
    assert_eq!(harness.port.posts(), ["원장 본문 ```x```"]);
}

#[tokio::test(start_paused = true)]
async fn local_retry_unreadable_root_or_child_has_no_further_post() {
    for unreadable_root in [true, false] {
        let harness = Harness::new();
        harness.gate.acquired();
        harness.port.say(BOT + 1, "readable");
        harness
            .port
            .replies
            .lock()
            .unwrap()
            .extend([Reply::Unsent, Reply::Unsent]);
        harness
            .port
            .unreadable
            .store(unreadable_root, Ordering::SeqCst);
        let mut writer = harness.writer();
        assert_eq!(writer.deliver(&piece("retry", "payload")).await, Step::Done);
        if !unreadable_root {
            let when = due(&mut writer);
            harness.port.unreadable.store(true, Ordering::SeqCst);
            assert_eq!(writer.retry_local(when).await, Step::Done);
            assert!(matches!(
                writer.store().ledger().piece(1).unwrap().outcome,
                Some(PieceOutcome::Unresolved(_))
            ));
        }
        assert!(!writer.has_local_retry());
        assert_eq!(
            harness.port.posts().len(),
            if unreadable_root { 1 } else { 2 }
        );
    }
}

struct GateHook(crate::services::tui_o::repost::admission::OriginalGate);

#[tokio::test(start_paused = true)]
async fn local_retry_original_rejections_keep_operator_resume_for_budget_and_unmarked_rows() {
    use crate::services::tui_o::repost::admission::OriginalGate;
    for marked in [false, true] {
        for status in [400, 403, 404] {
            let harness = Harness::new();
            harness.gate.acquired();
            harness
                .port
                .replies
                .lock()
                .unwrap()
                .push_back(Reply::Refused(status));
            let mut writer = harness
                .writer()
                .with_repost(Some(Box::new(GateHook(if marked {
                    OriginalGate::SendLocal
                } else {
                    OriginalGate::SendPgManaged
                }))));
            let item = piece("retry", "operator restored");
            assert_eq!(writer.deliver(&item).await, Step::Stopped);
            assert_eq!(
                writer.store().ledger().piece(0).unwrap().local_retry,
                marked.then_some(LocalRetryMark::Budget)
            );
            drop(writer);
            harness
                .store
                .record_operator_resume(CHANNEL, 0, "operator", "restored")
                .unwrap();
            let mut writer = harness.writer();
            assert_eq!(writer.deliver(&item).await, Step::Done);
            assert_eq!(
                harness.port.posts(),
                ["operator restored", "operator restored"]
            );
            assert_eq!(writer.store().ledger().piece(1).unwrap().local_retry, None);
        }
    }
}

impl crate::services::tui_o::repost::runner::RepostHook<Writer> for GateHook {
    fn turn<'a>(
        &'a mut self,
        _: &'a mut Writer,
    ) -> crate::services::tui_o::repost::runner::Turned<'a, ()> {
        Box::pin(std::future::ready(()))
    }
    fn original(
        &mut self,
        _: u64,
        _: crate::services::tui_o::store::ledger::PieceRecord,
    ) -> crate::services::tui_o::repost::runner::Turned<
        '_,
        crate::services::tui_o::repost::admission::OriginalGate,
    > {
        Box::pin(std::future::ready(self.0))
    }
    fn receipts(
        &mut self,
    ) -> crate::services::tui_o::repost::runner::Turned<
        '_,
        crate::services::tui_o::repost::projection::ReceiptOverlay,
    > {
        Box::pin(std::future::ready(
            crate::services::tui_o::repost::projection::ReceiptOverlay::Disabled,
        ))
    }
}

#[tokio::test(start_paused = true)]
async fn local_retry_budget_follows_explicit_owner_even_with_a_hook_present() {
    use crate::services::tui_o::repost::admission::OriginalGate;
    for owner in [
        OriginalGate::SendLocal,
        OriginalGate::SendPgManaged,
        OriginalGate::Admitted,
        OriginalGate::Unknown,
    ] {
        let harness = Harness::new();
        harness.gate.acquired();
        harness.port.say(BOT + 1, "readable");
        harness
            .port
            .replies
            .lock()
            .unwrap()
            .push_back(Reply::Unsent);
        let mut writer = harness
            .writer()
            .with_repost(Some(Box::new(GateHook(owner))));
        assert_eq!(
            writer.deliver(&piece("retry", "payload")).await,
            if owner == OriginalGate::Unknown {
                Step::LeaseBusy
            } else {
                Step::Done
            }
        );
        let expected = usize::from(matches!(
            owner,
            OriginalGate::SendLocal | OriginalGate::SendPgManaged
        ));
        assert_eq!(harness.port.posts().len(), expected);
        if expected == 1 {
            assert_eq!(
                writer.store().ledger().piece(0).unwrap().local_retry,
                (owner == OriginalGate::SendLocal).then_some(LocalRetryMark::Budget)
            );
            if owner == OriginalGate::SendLocal {
                let when = due(&mut writer);
                assert_eq!(writer.retry_local(when).await, Step::Done);
                assert_eq!(harness.port.posts(), ["payload", "payload"]);
            } else {
                assert!(!writer.has_local_retry());
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn input_notice_origin_survives_prepared_restart_and_local_retry() {
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let harness = Harness::new();
    harness.gate.acquired();
    harness.port.say(BOT + 1, "readable");
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .extend([Reply::Unsent, Reply::Created]);
    let (_bindings, item) = super::input_notice_transport_tests::materialized_notice(
        &harness,
        "native-input",
        "native input",
    );
    let origin = item.notice_origin.clone().unwrap();
    let mut writer = harness.writer();
    assert_eq!(
        writer.deliver(&Derived::Piece(item.clone())).await,
        Step::Done
    );
    assert_eq!(
        writer.store().ledger().piece(0).unwrap().notice_origin,
        Some(origin.clone())
    );
    drop(writer);
    let mut writer = harness.writer();
    let pending = local_retry::pending(writer.store().ledger()).expect("saved retry debt");
    assert_eq!(
        pending.piece, item,
        "retry must use the complete original provenance"
    );
    assert_eq!(writer.retry_local(pending.due).await, Step::Done);
    drop(writer);
    let store = harness.channel();
    assert_eq!(store.ledger().piece(1).unwrap().notice_origin, Some(origin));
    assert_eq!(harness.port.posts(), [item.payload.clone(), item.payload]);
}
