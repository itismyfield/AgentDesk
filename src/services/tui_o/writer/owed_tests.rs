//! The verified collector through the real `Sources` resume, follow and capture paths.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::watch;

use super::test_support::{
    Bindings, CHANNEL, Port, Writer, append, body, event, header, opened, owed_writer, payload,
    span, switched_on,
};
use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::store::codex_provenance::{CODEX_PROVENANCE_FILE, CodexProvenance};
use crate::services::tui_o::store::rotation::{BOUNDARY_FILE, Boundary};
use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_o::store::{OStore, STORE_DIR_NAME};
use crate::services::tui_o::writer::WriterAlarm;
use crate::services::tui_o::writer::actor::{POLL_INTERVAL, run_channel};
use crate::services::tui_o::writer::binding::BindingCause;
use crate::services::tui_o::writer::pieces::UnitDeriver;
use crate::services::tui_o::writer::rotation::Sources;
use crate::services::tui_prompt_dedupe::codex_verified::provenance::{
    CodexEpisodeDeny, CodexEpisodeSpan,
};

/// One actor's Codex sources and deriver, owing into the verified collector.
pub(crate) struct Collector {
    sources: Sources<Bindings>,
    deriver: UnitDeriver,
    pub log: OwedLog,
}

impl Collector {
    pub(crate) fn resumed(
        writer: &mut Writer,
        bindings: &Arc<Bindings>,
    ) -> Result<Self, WriterAlarm> {
        let (channel, codex) = (writer.channel(), ShadowProvider::Codex);
        let mut collector = Self {
            sources: Sources::new(channel, codex, Arc::clone(bindings)),
            deriver: UnitDeriver::new(channel, codex),
            log: OwedLog::new(channel, codex),
        };
        let (deriver, log) = (&mut collector.deriver, &mut collector.log);
        collector.sources.resume(writer, deriver, log)?;
        Ok(collector)
    }

    /// One actor read pass: binds past the checkpoint, then capture.
    fn read(&mut self, writer: &mut Writer) -> Result<(), WriterAlarm> {
        self.sources.follow(writer)?;
        (self.sources).capture(writer, &mut self.deriver, &mut self.log)
    }
}

/// A step the contract requires to complete without a writer halt.
pub(crate) fn kept<T>(result: Result<T, WriterAlarm>, step: &str) -> T {
    result.unwrap_or_else(|alarm| panic!("{step} halted the writer: {alarm:?}"))
}

fn transcript(beside: &Path, name: &str, session: &str, lines: &[String]) -> SourceId {
    let path = beside.with_file_name(name);
    std::fs::write(
        &path,
        [header(session)]
            .iter()
            .chain(lines)
            .cloned()
            .collect::<String>(),
    )
    .unwrap();
    source_id_for(session, &path).unwrap()
}

/// A Codex channel switched over `a.jsonl` after its session header, and no sidecar.
fn switched() -> (tempfile::TempDir, OStore, SourceId) {
    switched_on(CHANNEL)
}

fn channel_file(runtime: &tempfile::TempDir, file: &str) -> PathBuf {
    channel_file_on(runtime, CHANNEL, file)
}

fn channel_file_on(runtime: &tempfile::TempDir, channel: u64, file: &str) -> PathBuf {
    let dir = runtime.path().join(STORE_DIR_NAME);
    dir.join(channel.to_string()).join(file)
}

fn present(store: &ChannelStore) -> CodexProvenance {
    match store.codex_provenance() {
        Sidecar::Present(read) => read.raw().clone(),
        other => panic!("sidecar not present: {other:?}"),
    }
}

#[test]
fn rotation_writes_between_span_open_close_and_deny_keep_the_sidecar_and_never_halt() {
    let (runtime, store, a) = switched();
    let mut stale = span(&a, 0, Some(1));
    (stale.episode.user_message_id, stale.episode.native_turn_id) = (99, "n0".into());
    stale.episode.turn_nonce = "stale".into();
    let stale = serde_json::json!({"links": {}, "successors": {},
        "codex_spans": [stale], "codex_denies": []});
    let boundary = channel_file(&runtime, BOUNDARY_FILE);
    std::fs::write(&boundary, serde_json::to_vec(&stale).unwrap()).unwrap();
    let (mut writer, _) = owed_writer(opened(&store));
    let bindings = Bindings::new();
    bindings.bind(None, &a, BindingCause::Startup);
    let mut collector = kept(Collector::resumed(&mut writer, &bindings), "resume");
    writer.store().init_codex_provenance().unwrap();
    let start = std::fs::metadata(&a.path).unwrap().len();
    append(&a.path, &[event("task_started", "n1"), body("b1", "first")]);
    kept(collector.read(&mut writer), "capture");
    let open = span(&a, start, None);
    writer.store().persist_codex_span(open.clone()).unwrap();
    let b = transcript(&a.path, "b.jsonl", "s2", &[body("b2", "second")]);
    bindings.bind(Some(&a), &b, BindingCause::Clear);
    kept(collector.read(&mut writer), "bind between open and close");
    assert_eq!(present(writer.store()).spans, std::slice::from_ref(&open));
    append(&a.path, &[event("task_complete", "n1")]);
    kept(collector.read(&mut writer), "capture terminal");
    let mut closed = open;
    closed.end = Some(std::fs::metadata(&a.path).unwrap().len());
    writer.store().persist_codex_span(closed.clone()).unwrap();
    let c = transcript(&a.path, "c.jsonl", "s3", &[]);
    bindings.bind(Some(&b), &c, BindingCause::Clear);
    kept(collector.read(&mut writer), "bind between close and deny");
    let deny = CodexEpisodeDeny {
        execution: closed.execution.clone(),
        episode: closed.episode.clone(),
    };
    writer.store().persist_codex_deny(deny.clone()).unwrap();
    kept(collector.read(&mut writer), "capture after deny");
    drop(writer);
    let (mut writer, _) = owed_writer(opened(&store));
    let mut restarted = kept(Collector::resumed(&mut writer, &bindings), "restart");
    kept(restarted.read(&mut writer), "restarted capture");
    let sidecar = present(writer.store());
    assert_eq!(
        (sidecar.spans, sidecar.denies),
        (vec![closed.clone()], vec![deny])
    );
    let rotation = writer.store().rotation().unwrap();
    let links: Vec<_> = rotation
        .links
        .values()
        .map(|l| (&l.source, &l.boundary))
        .collect();
    let owed = Boundary::Owed { from: 0 };
    assert_eq!(links.len(), 2);
    assert!(links.contains(&(&b, &owed)) && links.contains(&(&c, &owed)));
    assert_eq!(rotation.successors[&source_key(&a)].source, b);
    assert_eq!(rotation.successors[&source_key(&b)].source, c);
    let rewritten = std::fs::read_to_string(&boundary).unwrap();
    assert!(
        !rewritten.contains("codex_spans"),
        "old additive fields are not carried"
    );
    let work = &mut collector.log.work;
    assert_eq!(
        work.iter().map(payload).collect::<Vec<_>>(),
        ["first", "second"]
    );
    reevaluate(writer.store(), work).unwrap();
    assert_eq!(work[0].origin.as_ref().map(|o| &o.span), Some(&closed));
    assert_eq!((&work[1].source, &work[1].origin), (&b, &None));
}

/// A unit read again from a second source derives nothing there, yet both sources stay
/// associated with its key.
#[test]
fn a_repeated_unit_keeps_every_source_it_was_owed_from() {
    let (_runtime, store, a) = switched();
    let lines = [body("shared", "same reply")];
    append(&a.path, &lines);
    let b = transcript(&a.path, "b.jsonl", "s2", &lines);
    let mut channel = opened(&store);
    channel.attach_source(&b).unwrap();
    for source in [&a, &b] {
        let through = channel.cursor(source).unwrap().captured_through;
        let mut capture = crate::services::tui_o::shadow::capture::SourceCapture::reopen(
            source.clone(),
            through,
            &channel.cursor(source).unwrap().prefix_hash,
        )
        .unwrap();
        let crate::services::tui_o::shadow::CaptureOutcome::Batch(batch) =
            crate::services::tui_o::shadow::CaptureSource::poll(&mut capture, 1 << 20)
        else {
            panic!("capture failed")
        };
        channel
            .append_spool(&batch, &capture.prefix_hash())
            .unwrap();
    }
    let (mut writer, _) = owed_writer(channel);
    let collector = kept(Collector::resumed(&mut writer, &Bindings::new()), "resume");
    assert_eq!(
        collector.log.work.len(),
        1,
        "the repeat derives no second piece"
    );
    let [(key, sources)] = collector.log.associations.iter().collect::<Vec<_>>()[..] else {
        panic!("one unit key expected")
    };
    assert_eq!(key.native_key, "shared");
    let mut named: Vec<_> = sources
        .iter()
        .map(|(source, _)| source_key(source))
        .collect();
    named.sort();
    let mut expected = vec![source_key(&a), source_key(&b)];
    expected.sort();
    assert_eq!(named, expected, "a derive-0 repeat lost its source");
}

/// The Claude-only empty-session rule never maps a Codex bind onto a stored cursor.
#[test]
fn a_codex_bind_with_an_empty_session_does_not_take_the_stored_cursor() {
    let (_runtime, store, a) = switched();
    let (mut writer, _) = owed_writer(opened(&store));
    let bindings = Bindings::new();
    bindings.bind(None, &a, BindingCause::Startup);
    let mut collector = kept(Collector::resumed(&mut writer, &bindings), "resume");
    kept(collector.read(&mut writer), "seed");
    let mut empty = a.clone();
    empty.session_id.clear();
    bindings.bind(Some(&a), &empty, BindingCause::Clear);
    kept(collector.read(&mut writer), "empty-session bind");
    assert!(
        writer.store().cursor(&empty).is_some(),
        "a Codex empty session was taken for the stored source"
    );
    assert!(writer.store().cursor(&a).is_some());
}

/// Production delivery neither reads, creates nor changes the sidecar: the same records post
/// the same way whether it is absent, valid with a closed span over them, or damaged.
#[tokio::test(start_paused = true)]
async fn the_running_actor_posts_the_same_and_leaves_the_sidecar_alone() {
    let mut seen = Vec::new();
    for sidecar in [None, Some(true), Some(false)] {
        let (runtime, store, a) = switched();
        let file = channel_file(&runtime, CODEX_PROVENANCE_FILE);
        let start = std::fs::metadata(&a.path).unwrap().len();
        let lines = [event("task_started", "n1"), body("b1", "first")];
        append(&a.path, &[lines.concat(), event("task_complete", "n1")]);
        let end = std::fs::metadata(&a.path).unwrap().len();
        let mut channel = opened(&store);
        match sidecar {
            Some(true) => {
                channel.init_codex_provenance().unwrap();
                channel
                    .persist_codex_span(span(&a, start, Some(end)))
                    .unwrap();
            }
            Some(false) => std::fs::write(&file, b"{damaged").unwrap(),
            None => {}
        }
        let before = std::fs::read(&file).ok();
        let (writer, port) = owed_writer(channel);
        let bindings = Bindings::new();
        bindings.bind(None, &a, BindingCause::Startup);
        let (stop, stopped) = watch::channel(false);
        let resumed = watch::channel(false).0;
        let codex = ShadowProvider::Codex;
        use crate::services::tui_o::store::codex_provenance::SIDECAR_IO;
        SIDECAR_IO.with(|io| io.set((0, 0)));
        let task = tokio::spawn(run_channel(writer, codex, bindings, stopped, resumed));
        tokio::time::sleep(POLL_INTERVAL * 4).await;
        stop.send(true).unwrap();
        assert_eq!(task.await.unwrap(), None, "the actor stopped: {sidecar:?}");
        let (reads, writes) = SIDECAR_IO.with(std::cell::Cell::get);
        assert_eq!(
            (reads, writes),
            (0, 0),
            "production sidecar I/O: {sidecar:?}"
        );
        assert_eq!(
            std::fs::read(&file).ok(),
            before,
            "sidecar touched: {sidecar:?}"
        );
        seen.push(port.posts());
    }
    assert_eq!(seen[0], ["first", "-# ✅ 완료"]);
    assert!(seen.iter().all(|posts| *posts == seen[0]), "{seen:?}");
}

#[path = "verified_delivery_tests.rs"]
pub(crate) mod verified;
