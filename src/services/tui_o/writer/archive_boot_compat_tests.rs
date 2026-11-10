use super::*;
use crate::services::tui_o::store::spool::source_key;

fn spool_next(store: &mut ChannelStore, capture: &mut SourceCapture) {
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("fixture must produce a capture batch")
    };
    assert!(!batch.records.is_empty());
    store.append_spool(&batch, &capture.prefix_hash()).unwrap();
}

#[test]
fn archive_boot_replays_later_source_announcements_before_retiring_latest_archive() {
    for readers in [false, true] {
        let (h, first_path, first, bindings) = fixture(b"");
        let second_path = first_path.with_file_name("second.jsonl");
        std::fs::write(&second_path, b"").unwrap();
        let second = source_id_for("second", &second_path).unwrap();
        let mut ordered = [first, second.clone()];
        ordered.sort_by_key(source_key);
        let [archived, later] = ordered;
        assert!(source_key(&archived) < source_key(&later));
        let mut archived_bytes = question();
        archived_bytes.extend(codex_body("completed", "already captured"));
        let unsealed = b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"AgentMessage\",\"id\":\"later-unsealed\"}}}\n";
        std::fs::write(&archived.path, &archived_bytes).unwrap();
        std::fs::write(&later.path, unsealed).unwrap();
        let mut store = h.channel();
        store.attach_source(&second).unwrap();
        for source in [&archived, &later] {
            let mut capture = SourceCapture::open(source.clone(), 0).unwrap();
            spool_next(&mut store, &mut capture);
        }
        for (seq, source) in [(2, second), (3, archived.clone())] {
            let mut event = bound(
                seq,
                None,
                BindingTarget::Source(source),
                BindingCause::Startup,
                None,
            );
            event.provider = ShadowProvider::Codex;
            bindings.commit(event);
        }
        store.set_binding_checkpoint(3).unwrap();
        let before = store.cursor(&archived).unwrap().clone();
        assert!(store.rotation().unwrap().links.is_empty());
        drop(store);
        compress_missing(&archived.path, &archived_bytes);
        let mut writer = h.writer();
        let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
        let result = sources(bindings, readers, true).resume(
            &mut writer,
            &mut deriver,
            &mut VecDeque::new(),
        );
        assert!(
            matches!(result, Err(WriterAlarm::HaltedMissingSource { .. })),
            "later source debt must hold latest archive: {readers} {result:?}"
        );
        assert!(deriver.has_unsealed(), "every source must have replayed");
        assert_eq!(writer.store().cursor(&archived), Some(&before));
        assert!(h.port.posts().is_empty());
    }
}

#[test]
fn archive_boot_preserves_recorded_drain_before_an_unapplied_rebinding() {
    for readers in [false, true] {
        for unknown in [false, true] {
            let captured = codex_body("captured", "captured prefix");
            let (h, path, old, bindings) = fixture(&captured);
            let mut writer = h.writer();
            let mut live = sources(bindings.clone(), readers, false);
            live.resume(
                &mut writer,
                &mut UnitDeriver::new(CHANNEL, ShadowProvider::Codex),
                &mut VecDeque::new(),
            )
            .unwrap();
            append(&path, &codex_body("unread", "known unread tail"));
            let drain_to = std::fs::metadata(&path).unwrap().len();
            let fresh_path = path.with_file_name("drain-successor.jsonl");
            std::fs::write(&fresh_path, b"").unwrap();
            let fresh = source_id_for("fresh", &fresh_path).unwrap();
            let mut event = bound(
                2,
                Some(&old),
                BindingTarget::Source(fresh.clone()),
                BindingCause::Startup,
                None,
            );
            event.provider = ShadowProvider::Codex;
            bindings.commit(event);
            live.follow(&mut writer).unwrap();
            assert_eq!(writer.store().binding_checkpoint().unwrap(), Some(2));
            let rotation = writer.store().rotation().unwrap();
            assert_eq!(
                rotation.successors[&source_key(&old)].drain_to,
                Some(drain_to)
            );
            let before = writer.store().cursor(&old).unwrap().clone();
            assert_eq!(before.captured_through, captured.len() as u64);
            assert!(drain_to > before.captured_through);
            let mut event = bound(
                3,
                Some(&fresh),
                BindingTarget::Source(old.clone()),
                BindingCause::Startup,
                None,
            );
            event.provider = ShadowProvider::Codex;
            bindings.commit(event);
            drop((live, writer));

            // An archive matching the cursor cannot erase an already observed unread tail.
            compress_missing(&path, &captured);
            if unknown {
                bindings.fail(Some("binding log temporarily unavailable"));
            }
            let mut writer = h.writer();
            let result = sources(bindings, readers, true).resume(
                &mut writer,
                &mut UnitDeriver::new(CHANNEL, ShadowProvider::Codex),
                &mut VecDeque::new(),
            );
            assert!(
                matches!(result, Err(WriterAlarm::HaltedMissingSource { detail, .. })
                    if detail.contains("capture obligation")),
                "recorded drain survives replay: readers={readers} unknown={unknown}"
            );
            assert_eq!(writer.store().cursor(&old), Some(&before));
            assert!(h.port.posts().is_empty());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn archive_boot_keeps_unposted_segments_without_gateway_then_collects_after_delivery() {
    for lost in [false, true] {
        let mut bytes = question();
        let (h, path, source, bindings) = fixture(&bytes);
        let mut store = h.channel();
        store.set_limits_for_test(1, u64::MAX);
        let cursor = store.cursor(&source).unwrap().clone();
        let mut capture =
            SourceCapture::reopen(source.clone(), cursor.captured_through, &cursor.prefix_hash)
                .unwrap();
        for (id, text) in [
            ("first", "first waiting reply"),
            ("second", "second waiting reply"),
        ] {
            let row = codex_body(id, text);
            append(&path, &row);
            bytes.extend(row);
            spool_next(&mut store, &mut capture);
        }
        assert_eq!(store.retained_segments(&source), 3);
        assert!(store.ledger().gc_segments(&source).is_empty());
        drop(capture);
        compress_missing(&path, &bytes);
        if lost {
            h.gate.lost();
        } else {
            h.gate.uncertain();
        }
        let (stop, task) = spawn_with(
            writer_over(&h, store),
            ShadowProvider::Codex,
            bindings.clone(),
        );
        polls(3).await;
        assert!(!task.is_finished(), "the archived reader must resume");
        assert!(h.port.posts().is_empty(), "no gateway permit: lost={lost}");
        let waiting = h.channel();
        assert!(waiting.cursor(&source).unwrap().retired);
        assert_eq!(waiting.ledger().next_serial(), 0);
        assert_eq!(waiting.retained_segments(&source), 3);
        assert!(
            waiting.ledger().gc_segments(&source).is_empty(),
            "unposted bodies must remain durable before ownership: lost={lost}"
        );
        drop(waiting);
        halt(stop, task).await;

        let (stop, task) = spawn_with(h.writer(), ShadowProvider::Codex, bindings.clone());
        polls(3).await;
        assert!(h.port.posts().is_empty(), "restart still has no permit");
        assert_eq!(h.channel().retained_segments(&source), 3);
        h.gate.acquired();
        polls(4).await;
        assert_eq!(
            h.port.posts(),
            ["first waiting reply", "second waiting reply"]
        );
        let collected = h.channel();
        assert_eq!(collected.ledger().next_serial(), 2);
        assert_eq!(collected.retained_segments(&source), 1);
        assert_eq!(collected.ledger().gc_segments(&source).len(), 2);
        assert!(collected.ledger().unresolved().is_none());
        assert!(collected.ledger().violation().is_none());
        drop(collected);
        halt(stop, task).await;

        let (stop, task) = spawn_with(h.writer(), ShadowProvider::Codex, bindings);
        polls(3).await;
        assert_eq!(
            h.port.posts(),
            ["first waiting reply", "second waiting reply"]
        );
        assert_eq!(h.channel().ledger().next_serial(), 2);
        assert_eq!(h.channel().retained_segments(&source), 1);
        halt(stop, task).await;
    }
}

#[test]
fn archive_boot_valid_unknown_current_preserves_the_captured_body() {
    for readers in [false, true] {
        for absent in [false, true] {
            let mut bytes = question();
            bytes.extend(codex_body("unknown-answer", "unknown current reply"));
            let (h, path, source, bindings) = fixture(&bytes);
            let mut expected = h.channel().cursor(&source).unwrap().clone();
            assert_eq!(expected.captured_through, bytes.len() as u64);
            compress_missing(&path, &bytes);
            bindings.fail(Some("binding log temporarily unavailable"));
            assert!(bindings.binding_events_since(CHANNEL, 0).is_err());
            let mut writer = h.writer();
            let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
            let mut owed = VecDeque::new();
            sources(bindings, readers, absent)
                .resume(&mut writer, &mut deriver, &mut owed)
                .unwrap();
            expected.retired = true;
            assert_eq!(writer.store().cursor(&source), Some(&expected));
            assert!(!deriver.has_unsealed());
            assert_eq!(owed.len(), 1);
            assert!(
                matches!(owed.front(), Some(Derived::Piece(piece))
                    if piece.unit_key.native_key == "unknown-answer"
                    && piece.unit_key.kind == UnitKind::Body
                    && piece.payload == "unknown current reply"),
                "archive proof must preserve the unposted body"
            );
            assert_eq!(writer.store().ledger().next_serial(), 0);
            assert!(h.port.posts().is_empty());
        }
    }
}

#[test]
fn archive_boot_keeps_live_fd_retirement_with_an_async_question() {
    for readers in [false, true] {
        let mut bytes = question();
        bytes.extend(codex_body("live-answer", "live captured reply"));
        let (h, path, source, bindings) = fixture(&bytes);
        let mut expected = h.channel().cursor(&source).unwrap().clone();
        let mut writer = h.writer();
        let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
        let mut owed = VecDeque::new();
        let mut live = sources(bindings, readers, false);
        live.resume(&mut writer, &mut deriver, &mut owed).unwrap();
        assert!(!deriver.has_unsealed());
        assert!(!writer.store().cursor(&source).unwrap().retired);
        assert_eq!(owed.len(), 1);
        let owed_before = owed.clone();
        compress_missing(&path, &bytes);
        live.capture(&mut writer, &mut deriver, &mut owed).unwrap();
        expected.retired = true;
        assert_eq!(writer.store().cursor(&source), Some(&expected));
        assert!(!deriver.has_unsealed());
        assert!(!live.is_parked(&source));
        assert_eq!(owed, owed_before, "live FD proof preserves queued output");
        assert_eq!(writer.store().ledger().next_serial(), 0);
        assert!(h.port.posts().is_empty());
    }
}
