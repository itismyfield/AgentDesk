use super::*;
use crate::services::tui_o::alarm::{AlarmHealth, AlarmRouter};
use crate::services::tui_o::writer::{ReaderConfig, rotation::Sources};
use std::time::SystemTime;

fn archive(path: &Path) {
    let mut name = path.as_os_str().to_os_string();
    name.push(".zst");
    // Deliberately invalid compressed bytes: classification must never read the archive.
    std::fs::write(PathBuf::from(name), b"existence only").unwrap();
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn drained_archive_retires_without_alarm_and_restarts_into_a_new_source() {
    for readers in [None, Some(ReaderConfig { dormant: true })] {
        let (h, path, old, _) = started(&row("old", "already consumed"));
        let health = Arc::new(AlarmHealth::default());
        let router = Arc::new(AlarmRouter::new(None, None, health.clone()));
        let mut w = ChannelWriter::new(
            h.channel(),
            h.gate.clone(),
            h.port.clone(),
            h.lease.clone(),
            router.clone(),
        );
        let bindings = Arc::new(FakeBindings::new());
        w.store().set_binding_checkpoint(0).unwrap();
        let mut s = Sources::new(CHANNEL, ShadowProvider::Codex, bindings.clone());
        if let Some(config) = readers {
            s = s.with_readers(config);
        }
        let mut d = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
        let mut owed = VecDeque::new();
        s.resume(&mut w, &mut d, &mut owed).unwrap();
        archive(&path);
        let read = s.capture(&mut w, &mut d, &mut owed);
        if let Err(alarm) = read.as_ref() {
            w.stop(alarm.clone());
        }
        assert!(read.is_ok(), "drained archive must not halt: {read:?}");
        assert!(w.store().cursor(&old).unwrap().retired);
        assert!(health.current_at(std::time::Instant::now()).is_empty());
        drop((s, w));
        let mut w = ChannelWriter::new(
            h.channel(),
            h.gate.clone(),
            h.port.clone(),
            h.lease.clone(),
            router,
        );
        let mut s = Sources::new(CHANNEL, ShadowProvider::Codex, bindings.clone());
        if let Some(config) = readers {
            s = s.with_readers(config);
        }
        s.resume(&mut w, &mut d, &mut owed).unwrap();
        let (fresh_path, fresh) = transcript(&path, "fresh.jsonl", "s2", b"");
        let mut bind = bound(
            1,
            Some(&old),
            BindingTarget::Source(fresh.clone()),
            BindingCause::Startup,
            None,
        );
        bind.provider = ShadowProvider::Codex;
        bindings.commit(bind);
        s.follow(&mut w).unwrap();
        append(&fresh_path, concat!(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"fresh\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"id\":\"fresh-answer\",\"content\":[{\"type\":\"output_text\",\"text\":\"fresh reply\"}]}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"fresh\"}}\n",
        ).as_bytes());
        s.capture(&mut w, &mut d, &mut owed).unwrap();
        while let Some(piece) = owed.pop_front() {
            w.deliver(&piece).await;
        }
        assert_eq!(h.port.posts(), ["fresh reply"]);
        assert!(health.current_at(std::time::Instant::now()).is_empty());
    }
}

#[test]
fn unread_tail_or_absent_archive_or_claude_keeps_missing_halt() {
    for readers in [None, Some(ReaderConfig { dormant: true })] {
        for case in ["unread", "partial", "absent", "claude"] {
            let (h, path, old, _) = started(&row("old", "consumed"));
            let provider = if case == "claude" {
                ShadowProvider::Claude
            } else {
                ShadowProvider::Codex
            };
            let mut w = h.writer();
            w.store().set_binding_checkpoint(0).unwrap();
            let mut s = Sources::new(CHANNEL, provider, Arc::new(FakeBindings::new()));
            if let Some(config) = readers {
                s = s.with_readers(config);
            }
            let mut d = UnitDeriver::new(CHANNEL, provider);
            let mut owed = VecDeque::new();
            s.resume(&mut w, &mut d, &mut owed).unwrap();
            let before = w.store().cursor(&old).unwrap().clone();
            if case == "unread" {
                append(&path, &row("tail", "must not lose"));
            }
            if case == "partial" {
                append(&path, b"{\"partial\":");
                s.capture(&mut w, &mut d, &mut owed).unwrap();
            }
            if case == "absent" {
                std::fs::remove_file(&path).unwrap();
            } else {
                archive(&path);
            }
            let alarm = s.capture(&mut w, &mut d, &mut owed).unwrap_err();
            assert!(
                matches!(&alarm, WriterAlarm::HaltedMissingSource { source, .. } if source == &old),
                "{case}: {alarm:?}"
            );
            assert_eq!(w.store().cursor(&old).unwrap(), &before, "{case}");
            let health = Arc::new(AlarmHealth::default());
            let router = AlarmRouter::new(None, None, health.clone());
            router.raise_at(CHANNEL, &alarm, std::time::Instant::now());
            assert_eq!(
                health.current_at(std::time::Instant::now()),
                [format!("tui_o:halted:{CHANNEL}")]
            );
        }
    }
}

#[test]
fn boot_archive_without_retained_fd_does_not_trust_a_prior_eof_witness() {
    for absent in [false, true] {
        for grows in [false, true] {
            let (h, path, old, _) = started(&row("old", "consumed"));
            let mut w = h.writer();
            w.store().set_binding_checkpoint(0).unwrap();
            w.store().prepare_cleanup_witnesses().unwrap();
            std::fs::File::open(&path)
                .unwrap()
                .set_modified(SystemTime::now() - std::time::Duration::from_secs(48 * 3600))
                .unwrap();
            let cursor = w.store().cursor(&old).unwrap().clone();
            let capture =
                SourceCapture::reopen(old.clone(), cursor.captured_through, &cursor.prefix_hash)
                    .unwrap();
            w.store()
                .write_cleanup_witness(
                    &crate::services::tui_o::store::cleanup_witness::CleanupWitness {
                        source: old.clone(),
                        captured_through: cursor.captured_through,
                        prefix_hash: cursor.prefix_hash.clone(),
                        fingerprint: capture
                            .verify_eof(cursor.captured_through, &cursor.prefix_hash)
                            .unwrap(),
                        observed_at: SystemTime::now(),
                    },
                )
                .unwrap();
            drop(capture);
            if grows {
                append(&path, &row("tail", "after witness before archive"));
            }
            archive(&path);
            let mut s = Sources::new(
                CHANNEL,
                ShadowProvider::Codex,
                Arc::new(FakeBindings::new()),
            )
            .with_readers(ReaderConfig { dormant: true })
            .with_producer_absent(absent);
            let alarm = s
                .resume(
                    &mut w,
                    &mut UnitDeriver::new(CHANNEL, ShadowProvider::Codex),
                    &mut VecDeque::new(),
                )
                .unwrap_err();
            assert!(matches!(alarm, WriterAlarm::HaltedMissingSource { .. }));
            assert_eq!(w.store().cursor(&old).unwrap(), &cursor);
        }
    }
}

#[tokio::test]
async fn archive_before_new_bind_retires_only_a_fulfilled_drain() {
    for readers in [None, Some(ReaderConfig { dormant: true })] {
        for unread in [false, true] {
            let (h, path, old, _) = started(&row("old", "consumed"));
            let bindings = Arc::new(FakeBindings::new());
            let mut w = h.writer();
            w.store().set_binding_checkpoint(0).unwrap();
            let mut s = Sources::new(CHANNEL, ShadowProvider::Codex, bindings.clone());
            if let Some(config) = readers {
                s = s.with_readers(config);
            }
            let mut d = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
            let mut owed = VecDeque::new();
            s.resume(&mut w, &mut d, &mut owed).unwrap();
            let cursor = w.store().cursor(&old).unwrap().clone();
            if unread {
                append(&path, &row("tail", "must not lose"));
            }
            archive(&path);
            let (fresh_path, fresh) = transcript(&path, "fresh.jsonl", "s2", b"");
            let mut event = bound(
                1,
                Some(&old),
                BindingTarget::Source(fresh.clone()),
                BindingCause::Startup,
                None,
            );
            event.provider = ShadowProvider::Codex;
            bindings.commit(event);
            s.follow(&mut w).unwrap();
            append(&fresh_path, concat!(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"fresh\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"id\":\"fresh-answer\",\"content\":[{\"type\":\"output_text\",\"text\":\"fresh reply\"}]}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"fresh\"}}\n",
            ).as_bytes());
            let result = s.capture(&mut w, &mut d, &mut owed);
            if unread {
                assert!(matches!(
                    result,
                    Err(WriterAlarm::HaltedMissingSource { .. })
                ));
                assert_eq!(w.store().cursor(&old).unwrap(), &cursor);
                assert!(h.port.posts().is_empty());
            } else {
                assert!(
                    result.is_ok(),
                    "fulfilled drain must not block retirement: {result:?}"
                );
                assert!(w.store().cursor(&old).unwrap().retired);
                while let Some(piece) = owed.pop_front() {
                    w.deliver(&piece).await;
                }
                assert_eq!(h.port.posts(), ["fresh reply"]);
            }
        }
    }
}
