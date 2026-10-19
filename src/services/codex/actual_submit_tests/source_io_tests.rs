//! Test-only consumer reads and cursor commits, keyed by the opened native file identity.

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

type Identity = (u64, u64);
type Samples = Arc<Mutex<Stats>>;
static WATCHED: LazyLock<Mutex<HashMap<Identity, Samples>>> = LazyLock::new(Default::default);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    pub read_calls: u64,
    pub read_bytes: u64,
    pub advance_commits: u64,
    pub events: Vec<(&'static str, u64, u64)>,
}

pub(crate) struct Watch {
    identity: Identity,
    samples: Samples,
}

impl Watch {
    pub(crate) fn start(path: &Path) -> Self {
        let metadata = std::fs::metadata(path).unwrap();
        let identity = (metadata.dev(), metadata.ino());
        let samples = Arc::new(Mutex::new(Stats::default()));
        assert!(
            WATCHED
                .lock()
                .unwrap()
                .insert(identity, Arc::clone(&samples))
                .is_none()
        );
        Self { identity, samples }
    }

    pub(crate) fn stats(&self) -> Stats {
        self.samples.lock().unwrap().clone()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let mut watched = WATCHED.lock().unwrap();
        if watched
            .get(&self.identity)
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.samples))
        {
            watched.remove(&self.identity);
        }
    }
}

fn samples(metadata: std::fs::Metadata) -> Option<Samples> {
    WATCHED
        .lock()
        .unwrap()
        .get(&(metadata.dev(), metadata.ino()))
        .cloned()
}

pub(crate) fn read(file: &File, kind: &'static str, bytes: u64) {
    if bytes == 0 {
        return;
    }
    let Some(samples) = file.metadata().ok().and_then(samples) else {
        return;
    };
    let mut stats = samples.lock().unwrap();
    stats.read_calls += 1;
    stats.read_bytes += bytes;
    stats.events.push((kind, bytes, 0));
}

pub(crate) fn advance(path: &Path, kind: &'static str, start: u64, end: u64) {
    if start == end {
        return;
    }
    let Some(samples) = std::fs::metadata(path).ok().and_then(samples) else {
        return;
    };
    let mut stats = samples.lock().unwrap();
    stats.advance_commits += 1;
    stats.events.push((kind, start, end));
}

#[test]
fn source_io_observes_real_checkpoint_reads_and_cross_thread_identity() {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new().unwrap();
    writeln!(file, "{{\"type\":\"response_item\"}}").unwrap();
    let watch = Watch::start(file.path());
    let path = file.path().to_owned();
    std::thread::spawn(move || {
        let mut reader = File::open(path).unwrap();
        let len = reader.metadata().unwrap().len();
        assert_eq!(
            crate::services::codex_tui::session::checkpoint::boundary(&mut reader, len).unwrap(),
            len
        );
    })
    .join()
    .unwrap();
    let stats = watch.stats();
    assert_eq!(stats.read_calls, 1);
    assert_eq!(stats.read_bytes, file.as_file().metadata().unwrap().len());
    assert_eq!(stats.advance_commits, 0);
    assert_eq!(stats.events[0].0, "checkpoint_boundary");
}

#[test]
fn source_io_observes_an_actual_runtime_cursor_commit() {
    use crate::services::{agent_protocol::RuntimeHandoffKind, tui_prompt_dedupe as dedupe};
    let _environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let path = root.path().join("native.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n",
    )
    .unwrap();
    let tmux = format!("source-io-{}", uuid::Uuid::new_v4().simple());
    dedupe::register_tmux_runtime_binding(
        &tmux,
        dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: Some(0),
        },
    );
    let watch = Watch::start(&path);
    let end = std::fs::metadata(&path).unwrap().len();
    assert!(dedupe::advance_tmux_runtime_binding_offset(
        &tmux,
        path.to_str().unwrap(),
        end
    ));
    assert_eq!(
        dedupe::runtime_binding_for_tmux_session(&tmux)
            .unwrap()
            .last_offset,
        end
    );
    let stats = watch.stats();
    assert_eq!(stats.advance_commits, 1);
    assert_eq!(stats.events, vec![("runtime_cursor_commit", 0, end)]);
    dedupe::clear_tmux_runtime_binding(&tmux);
}
