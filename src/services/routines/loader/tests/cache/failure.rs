use super::super::*;
use std::io::{self, Write};
use std::sync::Barrier;
use std::thread;
use tracing_subscriber::fmt::writer::MakeWriter;

#[derive(Clone)]
struct CapturingWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl Write for CapturingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturingWriter {
    type Writer = CapturingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn capture_debug_logs<F>(emit: F) -> String
where
    F: FnOnce(),
{
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .without_time()
        .with_writer(CapturingWriter {
            buffer: buffer.clone(),
        })
        .finish();
    tracing::subscriber::with_default(subscriber, emit);
    String::from_utf8(buffer.lock().unwrap().clone()).unwrap()
}
#[test]
fn source_failure_identity_uses_full_sha256() {
    let first = full_source_version("routine source one");
    let second = full_source_version("routine source two");

    assert_eq!(first.len(), 64);
    assert_eq!(second.len(), 64);
    assert_ne!(first, second);
    assert_eq!(first, full_source_version("routine source one"));
}

#[test]
fn failure_backoff_doubles_and_caps_at_one_hour() {
    let loader = RoutineScriptLoader::new().unwrap();
    let path = Path::new("broken.js");
    let version = Some("version-1".to_string());
    let now = Instant::now();

    let mut delays = Vec::new();
    for _ in 0..10 {
        delays.push(loader.record_failure(path, version.clone(), now));
    }

    assert_eq!(delays[0], Duration::from_secs(30));
    assert_eq!(delays[1], Duration::from_secs(60));
    assert_eq!(delays[2], Duration::from_secs(120));
    assert_eq!(delays[7], Duration::from_secs(60 * 60));
    assert_eq!(delays[9], Duration::from_secs(60 * 60));
}

#[test]
fn request_loader_reuses_runtime_lkg_during_backoff_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let roots = vec![dir.path().to_path_buf()];
    let runtime_root = test_runtime_root();
    let path = dir.path().join("request-routine.js");
    std::fs::write(
        &path,
        "agentdesk.routines.register({ name: 'Last Known Good', tick() { return { action: 'skip', reason: 'lkg' }; } });",
    )
    .unwrap();

    let runtime = RoutineScriptLoader::new_shared(&roots, &runtime_root).unwrap();
    assert_eq!(runtime.load_dirs(&roots).unwrap(), 1);
    std::fs::write(&path, "throw new Error('transient request failure');").unwrap();
    assert_eq!(runtime.load_dirs(&roots).unwrap(), 0);
    assert_eq!(
        runtime
            .state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );

    let request = RoutineScriptLoader::new_shared(&roots, &runtime_root).unwrap();
    assert!(Arc::ptr_eq(&runtime.state, &request.state));
    assert_eq!(request.load_dirs(&roots).unwrap(), 0);
    assert_eq!(
        request
            .state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        2,
        "a request during backoff must not re-evaluate the transient failure"
    );
    assert_eq!(
        request
            .get_script("request-routine.js")
            .unwrap()
            .unwrap()
            .name,
        "Last Known Good"
    );

    std::fs::write(
        &path,
        "agentdesk.routines.register({ name: 'Recovered Request', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    let recovered = RoutineScriptLoader::new_shared(&roots, &runtime_root).unwrap();
    assert_eq!(recovered.load_dirs(&roots).unwrap(), 1);
    assert_eq!(
        recovered
            .get_script("request-routine.js")
            .unwrap()
            .unwrap()
            .name,
        "Recovered Request"
    );
}

#[test]
fn concurrent_shared_loaders_singleflight_one_failure() {
    let dir = tempfile::tempdir().unwrap();
    let roots = vec![dir.path().to_path_buf()];
    let runtime_root = test_runtime_root();
    let path = dir.path().join("concurrent.js");
    std::fs::write(&path, "throw new Error('singleflight failure');").unwrap();

    let loaders = (0..8)
        .map(|_| Arc::new(RoutineScriptLoader::new_shared(&roots, &runtime_root).unwrap()))
        .collect::<Vec<_>>();
    let state = Arc::clone(&loaders[0].state);
    let barrier = Arc::new(Barrier::new(loaders.len()));
    let handles = loaders
        .into_iter()
        .map(|loader| {
            let roots = roots.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                loader.load_dirs(&roots).unwrap()
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        assert_eq!(handle.join().unwrap(), 0);
    }
    assert_eq!(
        state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    let failure = state
        .failed_scripts
        .lock()
        .unwrap()
        .get(&candidate_failure_key(&path))
        .unwrap()
        .clone();
    assert_eq!(failure.consecutive_failures, 1);
    let retry_delay = failure.retry_at.saturating_duration_since(Instant::now());
    assert!(retry_delay <= ROUTINE_LOAD_RETRY_BASE);
    assert!(retry_delay > Duration::ZERO);
}

#[test]
fn failed_script_backoff_skips_eval_until_due_and_content_change_bypasses_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recoverable.js");
    std::fs::write(&path, "throw new Error('broken');").unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
    assert_eq!(
        loader
            .state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
    assert_eq!(
        loader
            .state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "unchanged script must not be evaluated during backoff"
    );

    loader
        .state
        .failed_scripts
        .lock()
        .unwrap()
        .get_mut(&candidate_failure_key(&path))
        .unwrap()
        .retry_at = Instant::now();
    assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
    assert_eq!(
        loader
            .state
            .evaluation_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        2,
        "script must be retried after backoff expires"
    );

    std::fs::write(
        &path,
        "agentdesk.routines.register({ name: 'Recovered', tick() { return { action: 'skip' }; } });",
    )
    .unwrap();
    assert_eq!(loader.load_dir(dir.path()).unwrap(), 1);
    assert!(loader.state.failed_scripts.lock().unwrap().is_empty());
    assert_eq!(
        loader.get_script("recoverable.js").unwrap().unwrap().name,
        "Recovered"
    );
}

#[test]
fn read_failure_emits_one_error_during_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unreadable.js");
    std::fs::write(&path, [0xff]).unwrap();

    let loader = RoutineScriptLoader::new().unwrap();
    let read_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_attempts = Arc::clone(&read_attempts);
    *loader.source_read_observer.lock().unwrap() = Some(Arc::new(move |_| {
        observed_attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }));

    let logs = capture_debug_logs(|| {
        assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
        assert_eq!(loader.load_dir(dir.path()).unwrap(), 0);
    });
    assert_eq!(
        logs.matches("failed to read routine script; keeping last-known-good registry")
            .count(),
        1,
        "logs={logs}"
    );
    assert_eq!(
        loader
            .state
            .load_error_emissions
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "only the first failed read may emit an ERROR during backoff"
    );
    assert_eq!(read_attempts.load(std::sync::atomic::Ordering::Relaxed), 2);
    let failure = loader
        .state
        .failed_scripts
        .lock()
        .unwrap()
        .get(&candidate_failure_key(&path))
        .unwrap()
        .clone();
    assert_eq!(failure.source_version, None);
    assert_eq!(failure.consecutive_failures, 1);
}
