use std::cell::RefCell;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const CHILD_MARKER: &str = "ADK_ROOT_GUARD_TEARDOWN_CHILD";

thread_local! {
    static BEFORE_RESTORE: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}

pub(crate) fn before_restore(key: &str) {
    if key == "AGENTDESK_ROOT_DIR" {
        let callback = BEFORE_RESTORE.with(|hook| hook.borrow_mut().take());
        if let Some(callback) = callback {
            callback();
        }
    }
}

struct ClearHook;

impl Drop for ClearHook {
    fn drop(&mut self) {
        BEFORE_RESTORE.with(|hook| hook.borrow_mut().take());
    }
}

struct ResumeOnDrop(Option<mpsc::Sender<()>>);

impl ResumeOnDrop {
    fn resume(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

impl Drop for ResumeOnDrop {
    fn drop(&mut self) {
        self.resume();
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

pub(crate) fn assert_isolated<F, G>(test_name: &'static str, factory: F, root_was_present: bool)
where
    F: FnOnce() -> G + Send,
{
    run_isolated(
        test_name.split_once("::").unwrap().1,
        root_was_present,
        || exercise_teardown(factory, false, true),
    );
}

pub(crate) fn assert_restores_after_return(
    test_name: &'static str,
    root_was_present: bool,
    exercise: impl FnOnce(),
) {
    run_isolated(
        test_name.split_once("::").unwrap().1,
        root_was_present,
        || {
            let baseline = std::env::var_os("AGENTDESK_ROOT_DIR");
            exercise();
            let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
            assert_eq!(
                std::env::var_os("AGENTDESK_ROOT_DIR"),
                baseline,
                "actual test must restore the prior root on return"
            );
        },
    );
}

pub(crate) fn assert_scope_isolated(
    present: bool,
    temporary_root: bool,
    exercise: impl FnOnce() + Send,
) {
    let thread = std::thread::current();
    run_isolated(thread.name().unwrap(), present, || {
        exercise_teardown(exercise, true, temporary_root)
    });
}

fn run_isolated(test_name: &str, root_was_present: bool, exercise: impl FnOnce()) {
    if std::env::var(CHILD_MARKER).as_deref() == Ok(test_name) {
        exercise();
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
    if root_was_present {
        command.env("AGENTDESK_ROOT_DIR", baseline.path());
    } else {
        command.env_remove("AGENTDESK_ROOT_DIR");
    }
    let mut child = ReapChild(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "teardown probe watchdog expired");
        std::thread::sleep(Duration::from_millis(10));
    };
    use std::io::{Read, Seek, SeekFrom};
    let mut output = output;
    output.seek(SeekFrom::Start(0)).unwrap();
    let mut log = String::new();
    output.read_to_string(&mut log).unwrap();
    assert!(status.success(), "teardown probe failed:\n{log}");
    assert!(
        log.contains("1 passed; 0 failed; 0 ignored"),
        "missing child test:\n{log}"
    );
    println!("ADK_ROOT_TEARDOWN_CHILD=PASS test={test_name}");
}

fn exercise_teardown<F, G>(factory: F, whole_scope: bool, temporary_root: bool)
where
    F: FnOnce() -> G + Send,
{
    let baseline = std::env::var_os("AGENTDESK_ROOT_DIR");
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let actor = scope.spawn(move || {
            let _clear_hook = ClearHook;
            let mut arm = Some(|| {
                BEFORE_RESTORE.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        let _ = entered_tx.send(std::env::var_os("AGENTDESK_ROOT_DIR"));
                        resume_rx
                            .recv_timeout(Duration::from_secs(10))
                            .expect("resume teardown");
                    }));
                })
            });
            if whole_scope {
                arm.take().unwrap()();
            }
            let fixture = factory();
            if let Some(arm) = arm {
                arm();
            }
            drop(fixture);
        });
        let mut resume = ResumeOnDrop(Some(resume_tx));
        let root = entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("restore hook entered");
        let root_alive = root
            .as_deref()
            .is_some_and(|path| std::path::Path::new(path).is_dir());
        let held_until_restore = match crate::config::shared_test_env_lock().try_lock() {
            Err(std::sync::TryLockError::WouldBlock) => {
                resume.resume();
                actor.join().expect("fixture teardown");
                true
            }
            Ok(lock) => {
                let competing_root = tempfile::tempdir().unwrap();
                let competing_env = super::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                    "AGENTDESK_ROOT_DIR",
                    competing_root.path(),
                );
                resume.resume();
                actor.join().expect("fixture teardown");
                let observed = std::env::var_os("AGENTDESK_ROOT_DIR");
                drop(competing_env);
                drop(lock);
                assert_eq!(
                    observed.as_deref(),
                    Some(competing_root.path().as_os_str()),
                    "fixture restoration overwrote a concurrent locked owner's root"
                );
                false
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                panic!("unexpected poisoned environment lock")
            }
        };
        assert!(
            held_until_restore,
            "fixture released environment lock before restoration"
        );
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        assert_eq!(std::env::var_os("AGENTDESK_ROOT_DIR"), baseline);
        if temporary_root {
            assert!(
                root_alive,
                "temporary root must survive through environment restoration"
            );
            assert!(
                !std::path::Path::new(&root.unwrap()).exists(),
                "fixture temporary root must be removed after teardown"
            );
        }
    });
}
