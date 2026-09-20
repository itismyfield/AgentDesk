use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CHILD_MARKER: &str = "ADK_ENV_PANIC_PROBE_CHILD";
type Snapshot = Vec<(&'static str, Option<OsString>)>;

thread_local! {
    static ARMED: RefCell<Option<Snapshot>> = const { RefCell::new(None) };
}

struct ForcedPanic;
struct ClearProbe;

impl Drop for ClearProbe {
    fn drop(&mut self) {
        ARMED.with(|armed| armed.borrow_mut().take());
    }
}

pub(crate) fn checkpoint(expected: &[(&'static str, &OsStr)]) {
    let Some(baseline) = ARMED.with(|armed| armed.borrow_mut().take()) else {
        return;
    };
    assert_eq!(expected.len(), baseline.len(), "all fixture keys observed");
    for ((key, value), (prior_key, prior)) in expected.iter().zip(&baseline) {
        assert_eq!(key, prior_key, "fixture key order");
        let actual = std::env::var_os(key);
        assert_eq!(
            actual.as_deref(),
            Some(*value),
            "fixture override for {key}"
        );
        assert_ne!(&actual, prior, "fixture must change {key} before panic");
    }
    std::panic::panic_any(ForcedPanic);
}

struct ReapChild(Child);

impl Drop for ReapChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

pub(crate) fn assert_restores_after_panic(
    test_name: &'static str,
    keys: &[&'static str],
    prior_present: bool,
    exercise: impl FnOnce(),
) {
    let test_name = test_name.split_once("::").expect("crate-qualified test").1;
    if std::env::var(CHILD_MARKER).as_deref() == Ok(test_name) {
        exercise_unwind(keys, prior_present, exercise);
        return;
    }

    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let baseline = tempfile::tempdir().unwrap();
    let output = tempfile::tempfile().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([test_name, "--exact", "--nocapture"])
        .env(CHILD_MARKER, test_name)
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output.try_clone().unwrap()));
    for key in keys {
        if prior_present {
            command.env(key, baseline.path().join(key));
        } else {
            command.env_remove(key);
        }
    }
    let mut child = ReapChild(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "panic probe watchdog expired");
        std::thread::sleep(Duration::from_millis(10));
    };
    use std::io::{Read, Seek, SeekFrom};
    let mut output = output;
    output.seek(SeekFrom::Start(0)).unwrap();
    let mut log = String::new();
    output.read_to_string(&mut log).unwrap();
    assert!(status.success(), "panic probe failed:\n{log}");
    let summaries: Vec<_> = log
        .lines()
        .filter(|line| line.starts_with("test result:"))
        .collect();
    assert_eq!(summaries.len(), 1, "one child test summary:\n{log}");
    assert!(
        summaries[0].starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"),
        "child must execute one test:\n{log}"
    );
}

fn exercise_unwind(keys: &[&'static str], prior_present: bool, exercise: impl FnOnce()) {
    let baseline: Snapshot = keys
        .iter()
        .map(|key| (*key, std::env::var_os(key)))
        .collect();
    assert!(
        baseline
            .iter()
            .all(|(_, value)| value.is_some() == prior_present)
    );
    ARMED.with(|armed| *armed.borrow_mut() = Some(baseline.clone()));
    let _clear = ClearProbe;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(exercise));
    assert!(
        outcome.is_err_and(|payload| payload.is::<ForcedPanic>()),
        "actual fixture must reach the armed panic checkpoint"
    );
    assert!(ARMED.with(|armed| armed.borrow().is_none()));

    let (sender, receiver) = std::sync::mpsc::channel();
    let expected = baseline.clone();
    let verifier = std::thread::spawn(move || {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let actual: Snapshot = expected
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        sender.send(actual).unwrap();
    });
    let actual = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("next thread must acquire the environment mutex after unwind");
    verifier.join().expect("environment verifier");
    assert_eq!(actual, baseline, "panic must restore the prior environment");
}
