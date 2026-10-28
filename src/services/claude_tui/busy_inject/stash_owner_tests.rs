//! The A leaf under an owner, read from the transport trace: ready before any key, the payload
//! start just before content, and paste/Enter that only the pinned pane identity runs.
use super::*;
#[path = "s5_effect_tests.rs"]
mod s5_effect_tests;
use crate::services::claude_tui::input::submission::tests::{FAST, events, renderer, wait};
use crate::services::claude_tui::owner_input::{self, Commit, Hold, Pin};
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const TEXT: &str = "[User: test]\nowner leaf question";
/// Long enough for a test to replace the pane process between paste and Enter.
const SLOW: Timing = Timing {
    lock_retries: &[],
    settle: std::time::Duration::from_millis(1500),
    confirm_window: std::time::Duration::ZERO,
    confirm_poll: std::time::Duration::from_millis(10),
};

struct Rig {
    f: Fixture,
    name: String,
    pane_id: String,
    renderer: PathBuf,
    trace: PathBuf,
    fail_load: PathBuf,
    pane: Pane,
    nonce: String,
}

/// A distinct execution nonce per rig name.
fn hex(name: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    format!("{:016x}{:016x}", hasher.finish(), name.len())
}

/// Replaces the pane's process: same pane id, new pane_pid.
#[derive(Clone)]
struct Swap {
    real: PathBuf,
    socket: String,
    pane_id: String,
}

impl Swap {
    fn replace(&self) {
        let status = std::process::Command::new(&self.real)
            .args([
                "-L",
                &self.socket,
                "-f",
                "/dev/null",
                "respawn-pane",
                "-k",
                "-t",
            ])
            .arg(&self.pane_id)
            .status()
            .unwrap();
        assert!(status.success());
    }
}

impl Rig {
    fn new(name: &str) -> Self {
        let f = Fixture::after_shared_test_env_lock();
        let renderer = renderer(&f, name, "plain");
        let pane_id = f.tmux(&[
            "display-message",
            "-p",
            "-t",
            &format!("={name}:"),
            "#{pane_id}",
        ]);
        let trace = f.root.path().join(format!("{name}.trace"));
        let fail_load = f.root.path().join(format!("{name}.fail-load"));
        let shim = f.root.path().join(format!("{name}-traced-tmux"));
        fs::write(
            &shim,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in *load-buffer*) [ ! -f '{}' ] || exit 1 ;; esac\nexec '{}' \"$@\"\n",
                trace.display(),
                fail_load.display(),
                f.root.path().join("tmux").display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).unwrap();
        let pane = Pane::with_program(name, shim);
        let nonce = hex(name);
        let root = crate::services::tmux_common::current_tmux_owner_marker();
        let context = serde_json::json!({"schema":1,"provider":"claude","created_at":chrono::Utc::now(),"execution_nonce":&nonce,"tmux_session":name,"channel_id":742701,"owner_runtime_root":root,"host":"leaf-test","expected_native_session_id":null,"launch_mode":"fresh","provider_root":null});
        crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
            serde_json::from_value(context).unwrap(),
        )
        .unwrap();
        let marker = crate::services::tmux_common::session_temp_path(name, "spawn_nonce");
        fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        fs::write(marker, &nonce).unwrap();
        Self {
            f,
            name: name.into(),
            pane_id,
            renderer,
            trace,
            fail_load,
            pane,
            nonce,
        }
    }

    fn swap(&self) -> Swap {
        Swap {
            real: self.f.real.clone(),
            socket: self.f.socket.clone(),
            pane_id: self.pane_id.clone(),
        }
    }

    fn target(&self) -> serde_json::Value {
        let root = crate::services::tmux_common::current_tmux_owner_marker();
        serde_json::json!({"host":"tmux","owner":self.name,"root":root,"execution_nonce":self.nonce,"_attempt":"leaf-attempt"})
    }

    /// Pane writes and owner callbacks in the order the leaf issued them.
    fn steps(&self) -> Vec<String> {
        let text = fs::read_to_string(&self.trace).unwrap_or_default();
        text.lines()
            .filter_map(|line| {
                let step = if matches!(line, "pin" | "start") {
                    line
                } else if line.contains("load-buffer") {
                    "load"
                } else if line.contains("paste-buffer") {
                    "paste"
                } else if line.ends_with(" C-s") {
                    "C-s"
                } else if line.contains(" Enter") {
                    "Enter"
                } else {
                    return None;
                };
                Some(step.to_string())
            })
            .collect()
    }

    fn pinned_writes(&self) -> bool {
        let text = fs::read_to_string(&self.trace).unwrap_or_default();
        text.lines()
            .filter(|line| line.contains("paste-buffer") || line.contains(" Enter"))
            .all(|line| line.contains("if-shell") && line.contains(&self.pane_id))
    }

    fn events(&self) -> Vec<String> {
        std::thread::sleep(std::time::Duration::from_millis(300));
        events(&self.renderer)
    }

    /// Runs the leaf under an owner whose lease fails from the `lease_fails_at`-th pin and whose
    /// start commit fails when `start_fails`; `between` runs at the start commit.
    fn run(
        &self,
        timing: &Timing,
        stash: bool,
        lease_fails_at: usize,
        start_fails: bool,
        before: impl FnOnce(&Self),
        between: impl Fn() + Send + Sync + 'static,
    ) -> (Outcome, Option<String>, usize) {
        let starts = Arc::new(AtomicUsize::new(0));
        let pins = Arc::new(AtomicUsize::new(0));
        let trace = self.trace.clone();
        let note = move |event: &str| {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&trace)
                .unwrap();
            writeln!(file, "{event}").unwrap();
        };
        let counted = starts.clone();
        let started = note.clone();
        let commit: Commit = Arc::new(move |_| {
            started("start");
            counted.fetch_add(1, Ordering::SeqCst);
            between();
            if start_fails {
                Err("injected payload-start fault".into())
            } else {
                Ok(())
            }
        });
        let counted = pins.clone();
        let pin: Pin = Arc::new(move |_, _| {
            let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
            note("pin");
            if n >= lease_fails_at {
                Err("physical writer busy".into())
            } else {
                Ok(())
            }
        });
        let hold: Hold = Arc::new(|_| Ok(()));
        let (outcome, proof) = owner_input::capture_scope(
            "claude".into(),
            self.target(),
            742701,
            commit,
            pin,
            hold,
            || {
                before(self);
                submit(&self.pane, "4a0leaf", TEXT, timing, stash)
            },
        );
        let reason = proof.map(|proof| {
            proof
                .into_reason(&serde_json::json!("leaf-attempt"))
                .unwrap()
        });
        (outcome, reason, starts.load(Ordering::SeqCst))
    }
}

fn after_load(steps: &[String]) -> Vec<&str> {
    let at = steps.iter().position(|step| step == "load").expect("load");
    steps[at..].iter().map(String::as_str).collect()
}

/// With and without a parked draft the owner sees ready after the load and before any key, and
/// one payload start after C-s and before content; only the pinned identity takes paste/Enter.
#[test]
fn owner_leaf_orders_ready_and_start_around_the_optional_stash() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (stash, expected) in [
        (false, vec!["load", "pin", "start", "paste", "Enter"]),
        (true, vec!["load", "pin", "C-s", "start", "paste", "Enter"]),
    ] {
        let rig = Rig::new(&format!("a4-order-{stash}"));
        let (outcome, proof, starts) = rig.run(&FAST, stash, usize::MAX, false, |_| {}, || {});
        assert_eq!(outcome, Outcome::Injected, "stash={stash}");
        assert_eq!(after_load(&rig.steps()), expected, "stash={stash}");
        assert_eq!(starts, 1, "one payload clock, stash={stash}");
        assert!(proof.is_none());
        assert!(rig.pinned_writes(), "owner paste and Enter carry the pin");
        wait("a4-enter", || {
            events(&rig.renderer).iter().any(|e| e == "Enter")
        });
        assert_eq!(
            fs::read_to_string(rig.renderer.join("body")).unwrap(),
            TEXT,
            "stash={stash}"
        );
    }
    // Without an owner the same leaf sends main's plain paste and Enter.
    let rig = Rig::new("a4-unscoped");
    assert_eq!(
        submit(&rig.pane, "4a0plain", TEXT, &FAST, false),
        Outcome::Injected
    );
    assert_eq!(after_load(&rig.steps()), ["load", "paste", "Enter"]);
    let trace = fs::read_to_string(&rig.trace).unwrap();
    assert!(!trace.contains("if-shell"), "{trace}");
}

/// A process swapped after the pre-paste read, or a lease refused before C-s, leaves no key and
/// a not-sent proof naming the owner, kept apart from a failed load.
#[test]
fn owner_refusal_before_any_key_is_a_typed_not_sent() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = Rig::new("a4-swap-ready");
    let swap = rig.swap();
    let before = move |rig: &Rig| {
        assert!(super::super::before_paste(&rig.pane, true).is_ok());
        swap.replace();
    };
    let (outcome, proof, starts) = rig.run(&FAST, true, usize::MAX, false, before, || {});
    assert_eq!(outcome, Outcome::NotSent(Veto::PaneUnavailable));
    assert_eq!(after_load(&rig.steps()), ["load"]);
    assert_eq!(starts, 0);
    assert!(proof.unwrap().starts_with("owner_refused:"));
    assert!(rig.events().is_empty());

    let rig = Rig::new("a4-lease");
    // The scope's first pin is its observation; the second is the leaf's ready.
    let (outcome, proof, starts) = rig.run(&FAST, true, 2, false, |_| {}, || {});
    assert_eq!(outcome, Outcome::NotSent(Veto::LockContended));
    assert_eq!(after_load(&rig.steps()), ["load", "pin"]);
    assert_eq!(starts, 0);
    assert!(proof.unwrap().starts_with("owner_refused:"));
    assert!(rig.events().is_empty());

    let rig = Rig::new("a4-load");
    fs::write(&rig.fail_load, "").unwrap();
    let (outcome, proof, starts) = rig.run(&FAST, true, usize::MAX, false, |_| {}, || {});
    assert_eq!(outcome, Outcome::NotSent(Veto::LoadFailed));
    assert_eq!(after_load(&rig.steps()), ["load"]);
    assert_eq!((starts, proof.as_deref()), (0, Some("load_failed")));
}

/// Once C-s or content may have reached the pane no refusal is a not-sent proof: a failed start
/// after C-s is a stash failure, a swap before paste holds the paste, one after paste the Enter.
#[test]
fn owner_refusal_after_a_key_never_proves_not_sent() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = Rig::new("a4-start-fault");
    let (outcome, proof, starts) = rig.run(&FAST, true, usize::MAX, true, |_| {}, || {});
    assert_eq!(outcome, Outcome::Failed(WriteFailure::Stash));
    assert_eq!(after_load(&rig.steps()), ["load", "pin", "C-s", "start"]);
    assert_eq!((starts, proof), (1, None));
    assert_eq!(rig.events(), ["C-s"]);

    let rig = Rig::new("a4-swap-paste");
    let swap = rig.swap();
    let (outcome, proof, _) = rig.run(
        &FAST,
        false,
        usize::MAX,
        false,
        |_| {},
        move || swap.replace(),
    );
    assert_eq!(outcome, Outcome::Failed(WriteFailure::Paste));
    assert_eq!(proof, None);
    assert_eq!(after_load(&rig.steps()), ["load", "pin", "start", "paste"]);
    assert!(rig.events().is_empty(), "the new process takes no paste");

    let rig = Rig::new("a4-swap-enter");
    let swap = rig.swap();
    let body = rig.renderer.join("body");
    let between = move || {
        let (swap, body) = (swap.clone(), body.clone());
        std::thread::spawn(move || {
            wait("a4-pasted", || body.exists());
            swap.replace();
        });
    };
    let (outcome, proof, _) = rig.run(&SLOW, false, usize::MAX, false, |_| {}, between);
    assert_eq!(outcome, Outcome::Failed(WriteFailure::Enter));
    assert_eq!(proof, None);
    assert_eq!(
        after_load(&rig.steps()),
        ["load", "pin", "start", "paste", "Enter"]
    );
    assert_eq!(rig.events(), ["paste"], "one paste and no Enter");
}

/// A leaf on a thread without the owner's scope sends no key to the target that owner holds.
#[test]
fn an_unscoped_leaf_on_an_owner_held_target_sends_no_key() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = Rig::new("a4b-held-leaf");
    let shim = rig.f.root.path().join("a4b-held-leaf-traced-tmux");
    let name = rig.name.clone();
    let before = move |_: &Rig| {
        let (name, shim) = (name.clone(), shim.clone());
        let spawned = std::thread::spawn(move || {
            submit(
                &Pane::with_program(&name, shim),
                "4a0held",
                TEXT,
                &FAST,
                false,
            )
        });
        assert_eq!(
            spawned.join().unwrap(),
            Outcome::NotSent(Veto::LockContended)
        );
    };
    let (outcome, _, _) = rig.run(&FAST, false, usize::MAX, false, before, || {});
    assert_eq!(outcome, Outcome::Injected);
    let steps = rig.steps();
    assert_eq!(steps.iter().filter(|step| *step == "load").count(), 1);
    assert_eq!(
        after_load(&steps),
        ["load", "pin", "start", "paste", "Enter"]
    );
}
