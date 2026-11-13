use super::*;

use std::sync::Arc;
use tracing_subscriber::fmt::writer::MakeWriter;

#[derive(Clone)]
struct CapturingWriter(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for CapturingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> MakeWriter<'a> for CapturingWriter {
    type Writer = CapturingWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn io<F>(
    path: PathBuf,
    fsync: F,
) -> GenerationIo<
    impl FnOnce(&Path) -> Result<GenerationFileLock, String>,
    impl FnOnce(&Path) -> CounterRead,
    impl FnOnce(&Path, &str) -> Result<(), String>,
    F,
>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    GenerationIo {
        path: Some(path),
        lock: lock_generation_path,
        read: read_generation_counter,
        write: atomic_write,
        fsync,
        flushes: PARENT_DIR_FSYNC_FLUSHES,
    }
}

use super::ADVANCED_ROUTE_FOR_TESTS as ADVANCED;

fn expect(binding: ProcessGenerationAllocation, generation: u64, route: GenerationAllocationRoute) {
    assert_eq!(binding, ProcessGenerationAllocation { generation, route });
    assert_eq!(binding.epoch_route(), route.as_str());
    assert_eq!(
        binding.epoch_advanced(),
        matches!(route, GenerationAllocationRoute::AdvancedWithSyncedRename)
    );
}

fn fail_lock(_: &Path) -> Result<GenerationFileLock, String> {
    Err("lock".into())
}

fn panic_lock(_: &Path) -> Result<GenerationFileLock, String> {
    panic!("lock")
}

fn panic_read(_: &Path) -> CounterRead {
    panic!("read")
}

fn fail_write(_: &Path, _: &str) -> Result<(), String> {
    Err("write".into())
}

fn panic_write(_: &Path, _: &str) -> Result<(), String> {
    panic!("write")
}

fn panic_fsync(_: &Path) -> std::io::Result<()> {
    panic!("fsync")
}

#[test]
fn core_truth_table_routes_and_side_effects_are_exact() {
    let root = tempfile::tempdir().unwrap();
    let seeded = |name: &str, value: &str| {
        let path = root.path().join(name).join("generation");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, value).unwrap();
        path
    };
    let path = seeded("parent-failed", "7");
    expect(
        allocate_generation_epoch_with_io(io(path.clone(), |_| {
            Err(std::io::Error::from(std::io::ErrorKind::Other))
        })),
        8,
        GenerationAllocationRoute::ParentSyncFailed,
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "8");
    expect(
        allocate_generation_epoch_with_io(io(seeded("parsed", "7"), |_| Ok(()))),
        8,
        ADVANCED,
    );
    expect(
        allocate_generation_epoch_with_io(io(
            root.path().join("absent").join("generation"),
            |_| Ok(()),
        )),
        1,
        ADVANCED,
    );
    let path = seeded("unreadable", "nan");
    expect(
        allocate_generation_epoch_with_io(io(path.clone(), |_| Ok(()))),
        1,
        GenerationAllocationRoute::CounterReadFailed,
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "1");
    expect(
        allocate_generation_epoch_with_io(io(seeded("unreadable-parent", "nan"), |_| {
            Err(std::io::Error::from(std::io::ErrorKind::Other))
        })),
        1,
        GenerationAllocationRoute::ParentSyncFailed,
    );

    let path = seeded("lock-failed", "7");
    expect(
        allocate_generation_epoch_with_io(GenerationIo {
            path: Some(path.clone()),
            lock: fail_lock,
            read: read_generation_counter,
            write: panic_write,
            fsync: panic_fsync,
            flushes: PARENT_DIR_FSYNC_FLUSHES,
        }),
        7,
        GenerationAllocationRoute::LockFailed,
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "7");
    let path = seeded("write-failed", "7");
    expect(
        allocate_generation_epoch_with_io(GenerationIo {
            path: Some(path.clone()),
            lock: lock_generation_path,
            read: read_generation_counter,
            write: fail_write,
            fsync: panic_fsync,
            flushes: PARENT_DIR_FSYNC_FLUSHES,
        }),
        7,
        GenerationAllocationRoute::WriteFailed,
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "7");
    let path = seeded("saturated", &u64::MAX.to_string());
    expect(
        allocate_generation_epoch_with_io(io(path.clone(), |_| panic!("fsync"))),
        u64::MAX,
        GenerationAllocationRoute::Saturated,
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), u64::MAX.to_string());
    expect(
        allocate_generation_epoch_with_io(GenerationIo {
            path: None,
            lock: panic_lock,
            read: panic_read,
            write: panic_write,
            fsync: panic_fsync,
            flushes: PARENT_DIR_FSYNC_FLUSHES,
        }),
        0,
        GenerationAllocationRoute::PathUnavailable,
    );
}

/// Both advanced routes run on every host: only the injected flush
/// capability decides between them, and it never outranks a sync or read failure.
#[test]
fn injected_flush_capability_selects_the_advanced_route() {
    let root = tempfile::tempdir().unwrap();
    let seeded = |name: &str, value: &str| {
        let path = root.path().join(name).join("generation");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, value).unwrap();
        path
    };
    let with = |path: PathBuf, fsync_ok: bool, flushes: bool| {
        allocate_generation_epoch_with_io(GenerationIo {
            flushes,
            ..io(path, move |_: &Path| {
                if fsync_ok {
                    Ok(())
                } else {
                    Err(std::io::Error::from(std::io::ErrorKind::Other))
                }
            })
        })
    };
    use GenerationAllocationRoute as R;
    for (name, value, fsync_ok, flushes, generation, route) in [
        ("flushed", "7", true, true, 8, R::AdvancedWithSyncedRename),
        (
            "unflushed",
            "7",
            true,
            false,
            8,
            R::AdvancedWithUnflushedRename,
        ),
        (
            "unflushed-failed",
            "7",
            false,
            false,
            8,
            R::ParentSyncFailed,
        ),
        (
            "unflushed-unread",
            "nan",
            true,
            false,
            1,
            R::CounterReadFailed,
        ),
    ] {
        expect(
            with(seeded(name, value), fsync_ok, flushes),
            generation,
            route,
        );
    }
}

#[test]
fn production_shaped_once_cell_publishes_generation_and_route_together() {
    let cell = std::sync::OnceLock::new();
    let transaction = std::sync::Mutex::new(());
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let allocate = || {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ProcessGenerationAllocation {
            generation: 42,
            route: GenerationAllocationRoute::ParentSyncFailed,
        }
    };
    expect(
        allocate_once_with(&cell, &transaction, allocate),
        42,
        GenerationAllocationRoute::ParentSyncFailed,
    );
    expect(
        allocate_once_with(&cell, &transaction, allocate),
        42,
        GenerationAllocationRoute::ParentSyncFailed,
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn test_pin_memo_fallback_and_reset_remain_structurally_separate() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let path = generation_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    set_process_generation_for_tests(None);

    std::fs::write(&path, "7").unwrap();
    expect(allocate_process_generation_binding(), 8, ADVANCED);
    std::fs::write(&path, "19").unwrap();
    expect(
        process_generation_binding(),
        19,
        GenerationAllocationRoute::Unwitnessed,
    );
    assert_eq!(process_generation(), 19);

    set_process_generation_for_tests(Some(73));
    expect(
        allocate_process_generation_binding(),
        73,
        GenerationAllocationRoute::Unwitnessed,
    );
    expect(
        process_generation_binding(),
        73,
        GenerationAllocationRoute::Unwitnessed,
    );

    set_process_generation_for_tests(None);
    std::fs::write(&path, "40").unwrap();
    expect(
        process_generation_binding(),
        40,
        GenerationAllocationRoute::Unwitnessed,
    );
    expect(allocate_process_generation_binding(), 41, ADVANCED);
    set_process_generation_for_tests(None);
}

#[test]
fn production_composer_advances_with_synced_parent() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let path = root.path().join("runtime").join("generation");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "7").unwrap();

    expect(allocate_generation_epoch(), 8, ADVANCED);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "8");
}

#[test]
fn counter_read_failure_preserves_copy_detail() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(
        read_generation_counter(&root.path().join("missing")),
        CounterRead::Absent
    );
    let malformed = root.path().join("malformed");
    std::fs::write(&malformed, "nan").unwrap();
    assert_eq!(
        read_generation_counter(&malformed),
        CounterRead::Failed(CounterReadFailure::Parse)
    );
    let directory = root.path().join("directory");
    std::fs::create_dir(&directory).unwrap();
    assert!(matches!(read_generation_counter(&directory),
            CounterRead::Failed(CounterReadFailure::Io(kind))
                if kind != std::io::ErrorKind::NotFound));
}

fn capture_site_a(run: impl FnOnce()) -> String {
    let buffer = Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(CapturingWriter(buffer.clone()))
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    tracing::subscriber::with_default(subscriber, run);
    let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
    assert_eq!(logs.lines().count(), 1, "logs={logs}");
    logs
}

#[test]
fn site_a_emits_one_detailed_record_for_counter_read_failure() {
    if std::env::var_os("ADK_5927_GENERATION_CAPTURE_CHILD").is_none() {
        let qualified = format!(
            "{}::site_a_emits_one_detailed_record_for_counter_read_failure",
            module_path!().split_once("::").unwrap().1
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &qualified, "--nocapture"])
            .env("ADK_5927_GENERATION_CAPTURE_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "child must execute this exact test"
        );
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("generation");
    std::fs::write(&path, "nan").unwrap();
    let record = capture_site_a(|| {
        allocate_generation_epoch_with_io(io(path, |_| Ok(())));
    });
    for field in [
        "counter_read=\"parse_error\"",
        "counter_detail=\"parse\"",
        "route=\"counter_read_failed\"",
        "epoch_advanced=false",
    ] {
        assert!(record.contains(field), "{field}: {record}");
    }
}

#[test]
fn real_lock_failure_and_synced_advance_are_reachable() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("lock").join("generation");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "7").unwrap();
    std::fs::create_dir(path.with_extension("lock")).unwrap();
    expect(
        allocate_generation_epoch_with_io(io(path, |_| Ok(()))),
        7,
        GenerationAllocationRoute::LockFailed,
    );
    #[cfg(unix)]
    {
        let path = root.path().join("advanced").join("generation");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "7").unwrap();
        expect(
            allocate_generation_epoch_with_io(io(path, fsync_parent_dir)),
            8,
            GenerationAllocationRoute::AdvancedWithSyncedRename,
        );
    }
}
