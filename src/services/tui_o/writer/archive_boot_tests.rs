use super::*;
use crate::services::tui_o::writer::{ReaderConfig, rotation::Sources};

#[path = "archive_boot_compat_tests.rs"]
mod compatibility;

fn codex_body(id: &str, text: &str) -> Vec<u8> {
    let value = serde_json::json!({"type":"response_item", "payload":{
        "type":"message", "role":"assistant", "id":id, "phase":"final_answer",
        "content":[{"type":"output_text", "text":text}]}});
    format!("{value}\n").into_bytes()
}

fn question() -> Vec<u8> {
    let value = serde_json::json!({"type":"event_msg", "payload":{
        "type":"item_completed", "item":{"type":"AgentMessage", "id":"call_question",
        "phase":"final_answer", "delivery":"async", "questions":[{"title":"Which scope?"}]}}});
    format!("{value}\n").into_bytes()
}

fn modeled_bytes(length: usize, phantom: bool) -> Vec<u8> {
    let mut bytes = if phantom { question() } else { Vec::new() };
    bytes.extend(codex_body("answer", "already completed"));
    let prefix = b"{\"type\":\"progress\",\"padding\":\"";
    let suffix = b"\"}\n";
    while bytes.len() < length {
        let remaining = length - bytes.len();
        let chunk = if remaining > 65536 + prefix.len() + suffix.len() {
            65536
        } else {
            remaining
        };
        assert!(chunk >= prefix.len() + suffix.len());
        bytes.extend(prefix);
        bytes.resize(bytes.len() + chunk - prefix.len() - suffix.len(), b'x');
        bytes.extend(suffix);
    }
    assert_eq!(bytes.len(), length);
    bytes
}

fn fixture(bytes: &[u8]) -> (Harness, PathBuf, SourceId, Arc<FakeBindings>) {
    let (h, path, source) = switched_over(b"");
    h.gate.acquired();
    std::fs::write(&path, bytes).unwrap();
    let mut capture = SourceCapture::open(source.clone(), 0).unwrap();
    let mut store = h.channel();
    while capture.captured_through() < bytes.len() as u64 {
        let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
            panic!("synthetic transcript must capture")
        };
        store.append_spool(&batch, &capture.prefix_hash()).unwrap();
    }
    store.set_binding_checkpoint(1).unwrap();
    let bindings = Arc::new(FakeBindings::new());
    let mut event = bound(
        1,
        None,
        BindingTarget::Source(source.clone()),
        BindingCause::Startup,
        None,
    );
    event.provider = ShadowProvider::Codex;
    bindings.commit(event);
    (h, path, source, bindings)
}

fn compress_missing(path: &Path, bytes: &[u8]) -> PathBuf {
    let archive = path.with_extension("jsonl.zst");
    std::fs::write(&archive, zstd::stream::encode_all(bytes, 1).unwrap()).unwrap();
    std::fs::remove_file(path).unwrap();
    archive
}

fn sources(bindings: Arc<FakeBindings>, readers: bool, absent: bool) -> Sources<FakeBindings> {
    let sources =
        Sources::new(CHANNEL, ShadowProvider::Codex, bindings).with_producer_absent(absent);
    if readers {
        sources.with_readers(ReaderConfig { dormant: true })
    } else {
        sources
    }
}

async fn observed_shape(length: usize, phantom: bool) {
    let bytes = modeled_bytes(length, phantom);
    for readers in [false, true] {
        for absent in [false, true] {
            let (h, path, source, bindings) = fixture(&bytes);
            compress_missing(&path, &bytes);
            let before = h.channel().cursor(&source).unwrap().clone();
            let mut writer = h.writer();
            let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
            let mut owed = VecDeque::new();
            let mut reader = sources(bindings.clone(), readers, absent);
            reader.resume(&mut writer, &mut deriver, &mut owed).unwrap();
            assert!(writer.store().cursor(&source).unwrap().retired);
            assert_eq!(
                writer.store().cursor(&source).unwrap().captured_through,
                before.captured_through
            );
            assert_eq!(
                writer.store().cursor(&source).unwrap().prefix_hash,
                before.prefix_hash
            );
            assert!(!deriver.has_unsealed());
            while let Some(work) = owed.pop_front() {
                writer.deliver(&work).await;
            }
            assert_eq!(h.port.posts(), ["already completed"]);
            let serial = writer.store().ledger().next_serial();
            drop((writer, reader));
            std::fs::write(
                path.with_extension("jsonl.zst"),
                b"no proof after retirement",
            )
            .unwrap();
            let mut writer = h.writer();
            let mut reader = sources(bindings, readers, absent);
            let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
            reader.resume(&mut writer, &mut deriver, &mut owed).unwrap();
            assert!(owed.is_empty(), "Posted replay must not queue a duplicate");
            assert_eq!(writer.store().ledger().next_serial(), serial);
            assert_eq!(h.port.posts(), ["already completed"]);
        }
    }
}

#[tokio::test]
async fn archive_boot_observed_small_eof_retires_and_replays_posted_once() {
    observed_shape(175451, false).await;
}

#[tokio::test]
async fn archive_boot_observed_large_phantom_retires_and_replays_posted_once() {
    observed_shape(43609627, true).await;
}

#[test]
fn archive_boot_rejects_unread_and_corrupt_archives_without_cursor_mutation() {
    for readers in [false, true] {
        for case in [
            "extra",
            "partial",
            "frame",
            "hash",
            "short",
            "garbage",
            "truncated",
            "corrupt",
            "unsealed",
        ] {
            let mut bytes = codex_body("answer", "must remain captured");
            if case == "unsealed" {
                bytes.extend(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"AgentMessage\",\"id\":\"waiting-body\"}}}\n");
            }
            let (h, path, source, bindings) = fixture(&bytes);
            let before = h.channel().cursor(&source).unwrap().clone();
            let mut archived = bytes.clone();
            match case {
                "extra" => archived.push(b'\n'),
                "partial" => archived.extend(b"{\"unfinished\":"),
                "hash" => archived[0] = b'[',
                "short" => {
                    archived.pop();
                }
                _ => {}
            }
            let archive = compress_missing(&path, &archived);
            if case == "frame" || case == "garbage" {
                let mut compressed = std::fs::read(&archive).unwrap();
                if case == "frame" {
                    compressed.extend(zstd::stream::encode_all(&b"x"[..], 1).unwrap());
                } else {
                    compressed.extend(b"invalid trailing bytes");
                }
                std::fs::write(&archive, compressed).unwrap();
            } else if case == "truncated" {
                let mut compressed = std::fs::read(&archive).unwrap();
                compressed.pop();
                std::fs::write(&archive, compressed).unwrap();
            } else if case == "corrupt" {
                std::fs::write(&archive, b"not zstd").unwrap();
            }
            let mut writer = h.writer();
            let result = sources(bindings, readers, true).resume(
                &mut writer,
                &mut UnitDeriver::new(CHANNEL, ShadowProvider::Codex),
                &mut VecDeque::new(),
            );
            assert!(
                matches!(result, Err(WriterAlarm::HaltedMissingSource { .. })),
                "{case}: {result:?}"
            );
            assert_eq!(writer.store().cursor(&source), Some(&before), "{case}");
            assert!(h.port.posts().is_empty());
        }
    }
}

#[test]
fn archive_boot_rechecks_original_missing_at_both_clock_boundaries() {
    for readers in [false, true] {
        for boundary in [1, 2] {
            let bytes = codex_body("answer", "captured reply");
            let (h, path, source, bindings) = fixture(&bytes);
            compress_missing(&path, &bytes);
            let before = h.channel().cursor(&source).unwrap().clone();
            let calls = std::cell::Cell::new(0);
            let check_clock = || {
                let count = calls.get() + 1;
                calls.set(count);
                if count == boundary {
                    std::fs::write(&path, &bytes).unwrap();
                }
                std::time::SystemTime::now()
            };
            let mut writer = h.writer();
            let result = sources(bindings, readers, false).resume_with_clock(
                &mut writer,
                &mut UnitDeriver::new(CHANNEL, ShadowProvider::Codex),
                &mut VecDeque::new(),
                std::time::SystemTime::now(),
                &check_clock,
            );
            assert!(result.is_err());
            assert_eq!(calls.get(), boundary);
            assert_eq!(writer.store().cursor(&source), Some(&before));
            assert!(h.port.posts().is_empty());
        }
    }
}

#[tokio::test]
async fn archive_boot_resumes_after_a_fresh_bound_and_posts_its_first_reply_once() {
    for readers in [false, true] {
        let bytes = codex_body("old", "old reply");
        let (h, path, old, bindings) = fixture(&bytes);
        let archive = compress_missing(&path, &bytes);
        std::fs::write(&archive, b"invalid archive").unwrap();
        let mut writer = h.writer();
        assert!(matches!(
            sources(bindings.clone(), readers, false).resume(
                &mut writer,
                &mut UnitDeriver::new(CHANNEL, ShadowProvider::Codex),
                &mut VecDeque::new()
            ),
            Err(WriterAlarm::HaltedMissingSource { .. })
        ));
        let (fresh_path, fresh) = transcript(&path, "fresh.jsonl", "fresh", b"");
        let mut event = bound(
            2,
            Some(&old),
            BindingTarget::Source(fresh),
            BindingCause::Clear,
            None,
        );
        event.provider = ShadowProvider::Codex;
        bindings.commit(event);
        let mut reader = sources(bindings, readers, false);
        let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
        let mut owed = VecDeque::new();
        reader.resume(&mut writer, &mut deriver, &mut owed).unwrap();
        reader.follow(&mut writer).unwrap();
        while let Some(work) = owed.pop_front() {
            writer.deliver(&work).await;
        }
        append(
            &fresh_path,
            &codex_body("fresh-answer", "first fresh reply"),
        );
        reader
            .capture(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        while let Some(work) = owed.pop_front() {
            writer.deliver(&work).await;
        }
        reader
            .capture(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        assert!(owed.is_empty());
        assert_eq!(
            h.port
                .posts()
                .iter()
                .filter(|s| s.as_str() == "first fresh reply")
                .count(),
            1
        );
    }
}
