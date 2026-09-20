use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CHILD_MARKER: &str = "ADK_ENV_PANIC_PROBE_CHILD";
type Snapshot = Vec<(&'static str, Option<OsString>)>;

thread_local! {
    static ARMED: RefCell<Option<Probe>> = const { RefCell::new(None) };
}

struct ForcedPanic;
struct ClearProbe;

impl Drop for ClearProbe {
    fn drop(&mut self) {
        ARMED.with(|armed| armed.borrow_mut().take());
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Normal,
    Panic,
}

struct Probe {
    baseline: Snapshot,
    mode: Mode,
    seen: usize,
    valid: bool,
}

impl Probe {
    fn new(baseline: Snapshot, mode: Mode) -> Self {
        Self {
            baseline,
            mode,
            seen: 0,
            valid: false,
        }
    }

    fn observe(&mut self, expected: &Snapshot, actual: &Snapshot) {
        self.seen += 1;
        self.valid = false;
        assert_eq!(self.seen, 1, "exactly one fixture checkpoint");
        assert_eq!(
            expected.len(),
            self.baseline.len(),
            "all fixture keys observed"
        );
        assert_eq!(actual, expected, "exact fixture override");
        let mut changed = false;
        for ((key, value), (prior_key, prior)) in expected.iter().zip(&self.baseline) {
            assert_eq!(key, prior_key, "fixture key order");
            if value.is_some() {
                assert_ne!(value, prior, "fixture must change {key} before checkpoint");
            }
            changed |= value != prior;
        }
        assert!(changed, "fixture must change environment before checkpoint");
        self.valid = true;
    }

    fn finish(&self, outcome: std::thread::Result<()>) {
        assert_eq!(self.seen, 1, "actual fixture must reach one checkpoint");
        assert!(self.valid, "checkpoint validation must succeed");
        assert!(
            match self.mode {
                Mode::Normal => outcome.is_ok(),
                Mode::Panic => outcome.is_err_and(|payload| payload.is::<ForcedPanic>()),
            },
            "fixture must finish in the armed mode"
        );
    }
}

pub(crate) fn checkpoint(expected: &[(&'static str, &OsStr)]) {
    checkpoint_values(
        &expected
            .iter()
            .map(|(key, value)| (*key, Some(*value)))
            .collect::<Vec<_>>(),
    );
}

pub(crate) fn checkpoint_values(expected: &[(&'static str, Option<&OsStr>)]) {
    let mode = ARMED.with(|armed| {
        let mut armed = armed.borrow_mut();
        let probe = armed.as_mut()?;
        let expected: Snapshot = expected
            .iter()
            .map(|(key, value)| (*key, value.map(OsStr::to_os_string)))
            .collect();
        let actual = expected
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        probe.observe(&expected, &actual);
        Some(probe.mode)
    });
    if mode == Some(Mode::Panic) {
        std::panic::panic_any(ForcedPanic);
    }
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
    assert_restores(test_name, keys, prior_present, exercise, &[Mode::Panic]);
}

pub(crate) fn assert_root_restored(prior_present: bool, exercise: impl FnOnce()) {
    assert_restores_on_return_and_panic(&["AGENTDESK_ROOT_DIR"], prior_present, exercise);
}

pub(crate) fn assert_restores_on_return_and_panic(
    keys: &[&'static str],
    prior_present: bool,
    exercise: impl FnOnce(),
) {
    let thread = std::thread::current();
    let test_name = thread.name().expect("named libtest thread");
    assert_restores(
        test_name,
        keys,
        prior_present,
        exercise,
        &[Mode::Panic, Mode::Normal],
    );
}

fn assert_restores(
    test_name: &str,
    keys: &[&'static str],
    prior_present: bool,
    exercise: impl FnOnce(),
    modes: &[Mode],
) {
    if std::env::var(CHILD_MARKER).as_deref() == Ok(test_name) {
        let mode = match std::env::var("ADK_ENV_PROBE_MODE").as_deref() {
            Ok("normal") => Mode::Normal,
            Ok("panic") => Mode::Panic,
            _ => panic!("child mode missing"),
        };
        assert!(modes.contains(&mode));
        exercise_unwind(keys, prior_present, exercise, mode);
        return;
    }

    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for mode in modes {
        run_child(test_name, keys, prior_present, *mode);
    }
}

fn run_child(test_name: &str, keys: &[&str], prior_present: bool, mode: Mode) {
    let mode = match mode {
        Mode::Normal => "normal",
        Mode::Panic => "panic",
    };
    let baseline = tempfile::tempdir().unwrap();
    let output = tempfile::tempfile().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([test_name, "--exact", "--nocapture"])
        .env(CHILD_MARKER, test_name)
        .env("ADK_ENV_PROBE_MODE", mode)
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
    println!("ADK_ENV_PROBE_MODE={mode} PASS test={test_name}");
}

fn exercise_unwind(
    keys: &[&'static str],
    prior_present: bool,
    exercise: impl FnOnce(),
    mode: Mode,
) {
    let baseline: Snapshot = keys
        .iter()
        .map(|key| (*key, std::env::var_os(key)))
        .collect();
    assert!(
        baseline
            .iter()
            .all(|(_, value)| value.is_some() == prior_present)
    );
    ARMED.with(|armed| *armed.borrow_mut() = Some(Probe::new(baseline.clone(), mode)));
    let _clear = ClearProbe;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(exercise));
    ARMED.with(|armed| armed.borrow().as_ref().unwrap().finish(outcome));

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
    assert_eq!(
        actual, baseline,
        "fixture must restore the prior environment"
    );
}

fn check_protocol(mode: Mode) {
    let snapshot = |value: &str| vec![("ROOT", Some(OsString::from(value)))];
    let complete = || match mode {
        Mode::Normal => Ok(()),
        Mode::Panic => Err(Box::new(ForcedPanic) as Box<dyn std::any::Any + Send>),
    };
    let reject = |f: &mut dyn FnMut()| {
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err());
    };
    let mut cases = 0;
    for case in 0..8 {
        let mut probe = Probe::new(snapshot("before"), mode);
        let expected = snapshot("fixture");
        if case != 0 {
            match case {
                1 => {
                    probe.observe(&expected, &expected);
                    reject(&mut || probe.observe(&expected, &expected));
                }
                2 => reject(&mut || probe.observe(&expected, &snapshot("wrong"))),
                3 => reject(&mut || probe.observe(&snapshot("before"), &snapshot("before"))),
                4 => reject(&mut || probe.observe(&vec![("ROOT", None)], &expected)),
                5 => reject(&mut || {
                    probe.observe(
                        &vec![("OTHER", Some("fixture".into()))],
                        &vec![("OTHER", Some("fixture".into()))],
                    )
                }),
                6 => {
                    reject(&mut || probe.observe(&expected, &snapshot("wrong")));
                    reject(&mut || probe.observe(&expected, &expected));
                }
                7 => probe.observe(&expected, &expected),
                _ => unreachable!(),
            }
        }
        if case == 7 {
            reject(&mut || probe.finish(Err(Box::new("wrong payload"))));
            reject(&mut || {
                probe.finish(match mode {
                    Mode::Normal => Err(Box::new(ForcedPanic)),
                    Mode::Panic => Ok(()),
                })
            });
        } else {
            reject(&mut || probe.finish(complete()));
        }
        cases += 1;
    }
    assert_eq!(cases, 8);
    for optional in [false, true] {
        let mut baseline = snapshot("before");
        let mut expected = snapshot("fixture");
        if optional {
            baseline.push(("REMOVED", None));
            expected.push(("REMOVED", None));
        }
        let mut probe = Probe::new(baseline, mode);
        probe.observe(&expected, &expected);
        probe.finish(complete());
    }
}

#[test]
fn normal_probe_rejects_invalid_checkpoint_evidence() {
    check_protocol(Mode::Normal);
}

#[test]
fn panic_probe_rejects_invalid_checkpoint_evidence() {
    check_protocol(Mode::Panic);
}
