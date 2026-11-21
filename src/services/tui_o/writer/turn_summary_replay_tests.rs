use super::*;
use crate::services::tui_o::store::fault::{self, Keep, Step as At};

const SUMMARY_MARK: &str = "turn_summary_open";

fn captured(value: serde_json::Value) -> CapturedRecord {
    let line = serde_json::to_vec(&value).unwrap();
    CapturedRecord {
        start: 0,
        end: line.len() as u64 + 1,
        line,
    }
}

fn timestamp(second: u8) -> String {
    format!("2026-10-19T00:00:{second:02}Z")
}

fn prompt(id: &str, second: u8) -> CapturedRecord {
    captured(serde_json::json!({
        "type":"user", "uuid":format!("prompt-{id}"), "timestamp":timestamp(second),
        "message":{"content":"request"},
    }))
}

fn body(id: &str, second: u8) -> CapturedRecord {
    captured(serde_json::json!({
        "type":"assistant", "uuid":format!("uuid-{id}"), "apiBlockIndex":0,
        "timestamp":timestamp(second), "message":{"id":id, "usage":{"input_tokens":100},
            "content":[{"type":"text", "text":format!("answer {id}")}]},
    }))
}

fn idle(second: u8) -> CapturedRecord {
    captured(serde_json::json!({
        "type":"system", "subtype":"stop_hook_summary", "timestamp":timestamp(second),
    }))
}

fn records(id: &str, second: u8) -> [CapturedRecord; 3] {
    [prompt(id, second), body(id, second + 1), idle(second + 10)]
}

fn mark_key(id: &str) -> UnitKey {
    UnitKey {
        native_key: format!("{id}:0#turn"),
        kind: UnitKind::Tool,
        ..unit("")
    }
}

fn summary_key(id: &str) -> UnitKey {
    UnitKey {
        native_key: format!("{id}:0#turn_end"),
        kind: UnitKind::Tool,
        ..unit("")
    }
}

fn deriver(writer: &mut Writer) -> UnitDeriver {
    let ledger = writer.store().ledger();
    UnitDeriver::new(CHANNEL, ShadowProvider::Claude).with_turn_summary(pieces::SummaryInputs {
        turn_marks: ledger.summary_turn_marks(),
        disposed: ledger.disposed_unit_hashes(),
        context_window: Some(1000),
    })
}

fn summary(items: &[Derived]) -> PieceWork {
    let summaries = items
        .iter()
        .filter_map(|item| match item {
            Derived::Piece(piece) if piece.unit_key.native_key.ends_with("#turn_end") => {
                Some(piece.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 1, "{items:?}");
    summaries.into_iter().next().unwrap()
}

async fn deliver_all(writer: &mut Writer, items: &[Derived]) {
    for item in items {
        assert_eq!(writer.deliver(item).await, Step::Done, "{item:?}");
    }
}

async fn replay(writer: &mut Writer, records: &[CapturedRecord]) -> Vec<Derived> {
    let mut replay = deriver(writer);
    let mut derived = Vec::new();
    for record in records {
        let work = replay.derive(record);
        deliver_all(writer, &work).await;
        derived.extend(work);
    }
    derived
}

/// The request leaves, but a failed outcome append makes the next writer reconcile Prepared.
async fn crash_after_uncertain_post(harness: &Harness, writer: &mut Writer, piece: &PieceWork) {
    let ledger = ledger_path(harness);
    let planted = Arc::new(Mutex::new(None));
    let planting = Arc::clone(&planted);
    *harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
        let fault = fault::plant(
            &ledger,
            At::Append(Keep::Nothing),
            std::io::ErrorKind::StorageFull,
            Some(1),
        );
        *planting.lock().unwrap() = Some(fault);
    }));
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::CreatedUnseen);
    assert_eq!(
        writer.deliver(&Derived::Piece(piece.clone())).await,
        Step::Stopped
    );
    *harness.port.on_post.lock().unwrap() = None;
    drop(planted.lock().unwrap().take());
    let (_, prepared) = writer.store().ledger().unresolved().unwrap();
    assert_eq!(prepared.unit_key, piece.unit_key);
    assert_eq!(prepared.payload, piece.payload);
    assert_eq!(prepared.outcome, None);
}

#[tokio::test(start_paused = true)]
async fn old_posted_turns_without_marks_never_gain_summary_backfill() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let mut old = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    let turns = [
        records("old-a", 0),
        records("old-b", 20),
        records("old-c", 40),
    ];
    for turn in &turns {
        deliver_all(&mut writer, &old.derive(&turn[1])).await;
    }
    assert!(writer.store().ledger().summary_turn_marks().is_empty());
    let posts = harness.port.posts();
    let ledger = std::fs::read(ledger_path(&harness)).unwrap();
    drop(writer);
    let mut writer = harness.writer();
    let mut replay = deriver(&mut writer);
    for turn in &turns {
        for record in turn {
            let work = replay.derive(record);
            assert!(!work.iter().any(|item| matches!(item,
                Derived::Piece(piece) if piece.unit_key.native_key.ends_with("#turn_end"))));
            deliver_all(&mut writer, &work).await;
        }
    }
    assert_eq!(harness.port.posts(), posts);
    assert_eq!(std::fs::read(ledger_path(&harness)).unwrap(), ledger);
    assert!(writer.store().ledger().summary_turn_marks().is_empty());
    for id in ["old-a", "old-b", "old-c"] {
        assert!(
            writer
                .store()
                .ledger()
                .latest_piece(&summary_key(id), 0)
                .is_none()
        );
    }
    assert_eq!(writer.store().ledger().violation(), None);
}

#[tokio::test(start_paused = true)]
async fn summary_recovery_is_once_across_marker_body_prepared_and_posted_crashes() {
    for crash in [
        "before-u0",
        "marker-only",
        "body-posted",
        "prepared",
        "posted",
    ] {
        let harness = Harness::new();
        harness.gate.acquired();
        let records = records("first", 0);
        let mut writer = harness.writer();
        let mut before = deriver(&mut writer);
        assert!(before.derive(&records[0]).is_empty());
        if crash != "before-u0" {
            let first = before.derive(&records[1]);
            if crash == "marker-only" {
                assert!(matches!(&first[0], Derived::Excluded { unit_key, reason }
                    if *unit_key == mark_key("first") && reason == SUMMARY_MARK));
                deliver_all(&mut writer, &first[..1]).await;
                assert!(
                    writer
                        .store()
                        .ledger()
                        .latest_piece(&unit("first:0"), 0)
                        .is_none()
                );
            } else {
                deliver_all(&mut writer, &first).await;
            }
        }
        if matches!(crash, "prepared" | "posted") {
            let work = before.derive(&records[2]);
            let summary = summary(&work);
            if crash == "prepared" {
                crash_after_uncertain_post(&harness, &mut writer, &summary).await;
            } else {
                deliver_all(&mut writer, &work).await;
            }
        }
        drop((writer, before));
        let mut writer = harness.writer();
        let replayed = replay(&mut writer, &records).await;
        let summary = summary(&replayed);
        assert_eq!(summary.unit_key, summary_key("first"));
        assert_eq!(
            harness
                .port
                .posts()
                .iter()
                .filter(|payload| **payload == summary.payload)
                .count(),
            1,
            "{crash}"
        );
        assert_eq!(
            harness
                .port
                .posts()
                .iter()
                .filter(|payload| *payload == "answer first")
                .count(),
            1,
            "{crash}"
        );
        let (_, saved) = writer
            .store()
            .ledger()
            .latest_piece(&summary.unit_key, 0)
            .unwrap();
        assert_eq!(saved.payload, summary.payload);
        assert!(
            matches!(saved.outcome, Some(PieceOutcome::Posted(_))),
            "{crash}"
        );
        assert_eq!(
            writer.store().ledger().excluded(&mark_key("first")),
            Some(SUMMARY_MARK)
        );
        assert_eq!(writer.store().ledger().violation(), None, "{crash}");
        let posts = harness.port.posts();
        drop(writer);
        let mut writer = harness.writer();
        replay(&mut writer, &records).await;
        assert_eq!(harness.port.posts(), posts, "second restart: {crash}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_turn_mark_is_first_and_durable_before_u0_can_be_prepared() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let mut derive = deriver(&mut writer);
    let records = records("prefix", 0);
    assert!(derive.derive(&records[0]).is_empty());
    let prefix = derive.derive(&records[1]);
    assert!(matches!(prefix.as_slice(),
        [Derived::Excluded { unit_key, reason }, Derived::Piece(body)]
        if *unit_key == mark_key("prefix") && reason == SUMMARY_MARK
            && body.unit_key == unit("prefix:0")));
    deliver_all(&mut writer, &prefix[..1]).await;
    assert_eq!(writer.store().ledger().next_serial(), 0);
    assert_eq!(
        writer.store().ledger().excluded(&mark_key("prefix")),
        Some(SUMMARY_MARK)
    );
    assert!(
        writer
            .store()
            .ledger()
            .summary_turn_marks()
            .contains("prefix:0#turn")
    );
    assert!(harness.port.posts().is_empty());
    drop((writer, derive));
    let mut writer = harness.writer();
    replay(&mut writer, &records).await;
    assert_eq!(harness.port.posts().len(), 2);
    assert_eq!(harness.port.posts()[0], "answer prefix");
    assert!(matches!(
        writer
            .store()
            .ledger()
            .latest_piece(&summary_key("prefix"), 0)
            .unwrap()
            .1
            .outcome,
        Some(PieceOutcome::Posted(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn identical_summaries_keep_a_second_uncertain_prepared_ambiguous_without_resend() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let mut derive = deriver(&mut writer);
    let first = records("one", 0);
    for record in &first[..2] {
        deliver_all(&mut writer, &derive.derive(record)).await;
    }
    let first_summary = summary(&derive.derive(&first[2]));
    harness.port.unreadable.store(true, Ordering::SeqCst);
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::CreatedUnseen);
    assert_eq!(
        writer.deliver(&Derived::Piece(first_summary.clone())).await,
        Step::Done
    );
    harness.port.unreadable.store(false, Ordering::SeqCst);
    assert!(matches!(
        writer
            .store()
            .ledger()
            .latest_piece(&first_summary.unit_key, 0)
            .unwrap()
            .1
            .outcome,
        Some(PieceOutcome::Unresolved(_))
    ));
    let second = records("two", 20);
    for record in &second[..2] {
        deliver_all(&mut writer, &derive.derive(record)).await;
    }
    let second_summary = summary(&derive.derive(&second[2]));
    assert_eq!(first_summary.payload, second_summary.payload);
    assert_ne!(first_summary.unit_key, second_summary.unit_key);
    crash_after_uncertain_post(&harness, &mut writer, &second_summary).await;
    let posts = harness.port.posts();
    assert_eq!(
        posts
            .iter()
            .filter(|payload| **payload == second_summary.payload)
            .count(),
        2
    );
    drop((writer, derive));
    let mut writer = harness.writer();
    assert_eq!(
        writer
            .deliver(&Derived::Piece(second_summary.clone()))
            .await,
        Step::Done
    );
    let (serial, saved) = writer
        .store()
        .ledger()
        .latest_piece(&second_summary.unit_key, 0)
        .unwrap();
    assert!(
        matches!(&saved.outcome, Some(PieceOutcome::Ambiguous(candidates)) if candidates.len() == 1)
    );
    assert!(
        harness
            .alarms
            .taken()
            .contains(&WriterAlarm::Ambiguous { serial })
    );
    assert_eq!(
        writer.deliver(&Derived::Piece(second_summary)).await,
        Step::Done
    );
    let both = first.into_iter().chain(second).collect::<Vec<_>>();
    replay(&mut writer, &both).await;
    assert_eq!(
        harness.port.posts(),
        posts,
        "an ambiguous summary must not be blindly re-sent"
    );
    assert!(!writer.is_stopped());
    assert_eq!(writer.store().ledger().violation(), None);
}

#[tokio::test(start_paused = true)]
async fn a_disposed_retained_tail_without_its_u0_mark_never_backfills_after_gc() {
    for already_posted in [false, true] {
        let harness = Harness::new();
        harness.gate.acquired();
        let mut writer = harness.writer();
        let mut derive = deriver(&mut writer);
        for record in [prompt("gc", 0), body("gc-first", 1), body("gc-tail", 2)] {
            deliver_all(&mut writer, &derive.derive(&record)).await;
        }
        assert_eq!(
            writer.store().ledger().excluded(&mark_key("gc-first")),
            Some(SUMMARY_MARK)
        );
        if already_posted {
            deliver_all(&mut writer, &derive.derive(&idle(10))).await;
        }
        let posts = harness.port.posts();
        let ledger = std::fs::read(ledger_path(&harness)).unwrap();
        drop((writer, derive));
        let mut writer = harness.writer();
        let retained = [body("gc-tail", 2), idle(10)];
        let derived = replay(&mut writer, &retained).await;
        assert!(!derived.iter().any(|item| matches!(item,
            Derived::Piece(piece) if piece.unit_key.native_key.ends_with("#turn_end"))));
        assert_eq!(harness.port.posts(), posts);
        assert_eq!(std::fs::read(ledger_path(&harness)).unwrap(), ledger);
        let state = writer.store().ledger();
        assert!(state.excluded(&mark_key("gc-tail")).is_none());
        assert!(state.latest_piece(&summary_key("gc-tail"), 0).is_none());
        assert_eq!(
            state.latest_piece(&summary_key("gc-first"), 0).is_some(),
            already_posted
        );
        assert_eq!(state.violation(), None);
    }
}

#[tokio::test(start_paused = true)]
async fn prepared_old_footer_reconciles_using_its_saved_bytes_after_format_change() {
    let harness = Harness::new();
    harness.gate.acquired();
    let records = records("old-footer", 0);
    let mut writer = harness.writer();
    let mut derive = deriver(&mut writer);
    derive.derive(&records[0]);
    deliver_all(&mut writer, &derive.derive(&records[1])).await;
    let mut prepared = summary(&derive.derive(&records[2]));
    prepared.payload = "-# ⏱️ 10초 · 📦 10%".into();
    crash_after_uncertain_post(&harness, &mut writer, &prepared).await;
    drop(writer);

    let mut writer = harness.writer();
    let new_payload = summary(&replay(&mut writer, &records).await).payload;
    assert_eq!(new_payload, "-# ⏱️ 10초 (09:00) · 📦 10%");
    assert_eq!(
        harness.port.posts(),
        ["answer old-footer", &prepared.payload]
    );
    let (_, saved) = writer
        .store()
        .ledger()
        .latest_piece(&prepared.unit_key, 0)
        .unwrap();
    assert_eq!(saved.payload, prepared.payload);
    assert!(matches!(saved.outcome, Some(PieceOutcome::Posted(_))));
}

pub(crate) struct FooterWriter {
    harness: Harness,
    writer: Writer,
}

impl FooterWriter {
    pub(crate) fn new(channel: u64, pool: sqlx::PgPool) -> Self {
        let harness = Harness::build_channel(channel, |_| Vec::new());
        harness.gate.acquired();
        harness.port.usage_footer_pool.set(pool).unwrap();
        let writer = harness.writer();
        Self { harness, writer }
    }

    pub(crate) fn root(&self) -> &std::path::Path {
        self.harness._runtime.path()
    }

    pub(crate) fn restart(&mut self) {
        self.writer = self.harness.writer();
    }

    pub(crate) async fn turn(&mut self, id: &str) -> String {
        use crate::services::claude_tui::hook_bundle::statusline::{record_dir, record_path};

        let session = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let root = record_dir();
        std::fs::create_dir_all(&root).unwrap();
        let path = record_path(&root, &session);
        std::fs::write(
            &path,
            serde_json::json!({"session_id":session, "rate_limits":{
                "five_hour":{"used_percentage":24,"resets_at": now.timestamp()+18000}
            }})
            .to_string(),
        )
        .unwrap();
        let ledger = self.writer.store().ledger();
        let mut derive = UnitDeriver::new(self.harness.channel_id, ShadowProvider::Claude)
            .with_turn_summary(pieces::SummaryInputs {
                turn_marks: ledger.summary_turn_marks(),
                disposed: ledger.disposed_unit_hashes(),
                context_window: None,
            });
        let [prompt, body, terminal] = records(id, 0).map(|record| {
            let mut value: serde_json::Value = serde_json::from_slice(&record.line).unwrap();
            value["sessionId"] = serde_json::json!(session);
            captured(value)
        });
        let before = self.harness.port.posts().len();
        for record in [prompt, body, terminal] {
            deliver_all(&mut self.writer, &derive.derive(&record)).await;
        }
        let posts = self.harness.port.posts();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(posts.len(), before + 2, "{posts:?}");
        assert_eq!(posts[before], format!("answer {id}"));
        posts[before + 1].clone()
    }
}

#[tokio::test(start_paused = true)]
async fn unprepared_replay_uses_model_only_from_its_own_turn_interval() {
    use crate::services::claude_tui::hook_bundle::statusline::{record_dir, record_path};

    let harness = Harness::new();
    harness.gate.acquired();
    let root = record_dir();
    std::fs::create_dir_all(&root).unwrap();
    let now = Utc::now();
    let started = now - chrono::Duration::seconds(90);
    let ended = now - chrono::Duration::seconds(80);
    for (id, after_start, owned) in [
        ("later-statusline", 88, false),
        ("settled-statusline", 13, true),
        ("in-turn-statusline", 5, true),
    ] {
        let observed_at = started + chrono::Duration::seconds(after_start);
        let session = uuid::Uuid::new_v4().to_string();
        let rows = records(id, 0);
        let rows = std::array::from_fn::<_, 3, _>(|i| {
            let mut value: serde_json::Value = serde_json::from_slice(&rows[i].line).unwrap();
            value["sessionId"] = serde_json::json!(session);
            value["timestamp"] = serde_json::json!(if i < 2 { started } else { ended });
            captured(value)
        });
        let mut writer = harness.writer();
        let mut live = deriver(&mut writer);
        for row in &rows[..2] {
            deliver_all(&mut writer, &live.derive(row)).await;
        }
        assert_eq!(
            writer.store().ledger().excluded(&mark_key(id)),
            Some(SUMMARY_MARK)
        );
        assert!(
            writer
                .store()
                .ledger()
                .latest_piece(&summary_key(id), 0)
                .is_none()
        );
        drop((live, writer));

        let path = record_path(&root, &session);
        std::fs::write(&path, serde_json::json!({
            "session_id":session, "model":{"display_name":"Opus 4.6"},
            "effort":{"level":"high"}, "fast_mode":true,
            "rate_limits":{"five_hour":{"used_percentage":24,"resets_at": now.timestamp()+18000}}
        }).to_string()).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(
                std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_millis(observed_at.timestamp_millis() as u64),
            )
            .unwrap();
        let mut reopened = harness.writer();
        let footer = summary(&replay(&mut reopened, &rows).await);
        std::fs::remove_file(path).unwrap();
        assert_eq!(
            footer.payload.contains("Opus 4.6 (high, fast)"),
            owned,
            "{}",
            footer.payload
        );
        assert!(
            footer.payload.contains("\n-# 5h 24% ("),
            "{}",
            footer.payload
        );
        let answer = format!("answer {id}");
        assert_eq!(
            harness
                .port
                .posts()
                .iter()
                .filter(|body| **body == answer)
                .count(),
            1
        );
    }
}
