//! An Enter refused on a transient pane gap after its paste is judged whole again, only on the
//! paste's server, at most three times within a second; every other refusal ends the plan.
use super::*;
use crate::services::claude_tui::host_input::watch_actions;
use crate::services::provider::CancelToken;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicU32, AtomicUsize};

type Env = fn(&std::path::Path) -> Vec<String>;

/// What the pane does once the paste lands.
struct After {
    armed: AtomicBool,
    /// Provider start reads that fail.
    unreadable: AtomicUsize,
    /// One environment read.
    env: Mutex<Option<Env>>,
    /// The server later dials reach.
    server: AtomicU32,
    /// Where the turn is cancelled.
    cancel_at: &'static str,
    /// `pane.process_info` replies the server withholds from the first gap on.
    withhold: usize,
    /// Another holder of the target writes an Escape.
    other_write: bool,
    token: CancelToken,
}

fn after(unreadable: usize, env: Option<Env>, server: u32) -> After {
    After {
        armed: false.into(),
        unreadable: unreadable.into(),
        env: Mutex::new(env),
        server: server.into(),
        cancel_at: "",
        withhold: 0,
        other_write: false,
        token: CancelToken::new(),
    }
}

impl After {
    /// Whether the plan reads the pane after the paste; a cancel set for `at` lands here.
    fn reached(&self, at: &str) -> bool {
        let armed = self.armed.load(SeqCst);
        self.token
            .cancelled
            .fetch_or(armed && self.cancel_at == at, SeqCst);
        armed
    }
}

struct Os(FakeOs, Arc<After>, PathBuf, Arc<AtomicUsize>);

impl ProcessOs for Os {
    fn parent(&self, pid: u32) -> Result<u32, RestoreUnverified> {
        self.0.parent(pid)
    }
    fn start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        let failing = |n: usize| n.checked_sub(1);
        let armed = pid == PROVIDER && self.1.armed.load(SeqCst);
        match armed
            && self
                .1
                .unreadable
                .fetch_update(SeqCst, SeqCst, failing)
                .is_ok()
        {
            true if self.1.reached("gap") => {
                self.3.fetch_add(self.1.withhold, SeqCst);
                Err(RestoreUnverified::ProcessUnreadable)
            }
            _ => self.0.start(pid),
        }
    }
    fn environ(&self, pid: u32) -> Result<Vec<String>, RestoreUnverified> {
        let env = self
            .1
            .reached("env")
            .then(|| self.1.env.lock().unwrap().take());
        env.flatten()
            .map_or_else(|| self.0.environ(pid), |env| Ok(env(&self.2)))
    }
    fn exec_path(&self, pid: u32) -> Option<String> {
        self.0.exec_path(pid)
    }
}

/// Runs `plans` on one target of the production chain; `after` takes hold once a paste lands.
/// A withheld reply costs a read its 900 ms socket timeout before the retry that succeeds.
fn run_plans(after: After, plans: &[&[TuiInputAction]]) -> (InputRun, Vec<Value>, InputTarget) {
    let (rig, after) = (rig(), Arc::new(after));
    let (dialled, armed) = (after.clone(), after.clone());
    let server = after.server.swap(SERVER, SeqCst);
    let peers: PeerReader = Box::new(move |_| Ok((dialled.server.load(SeqCst), start(1_000))));
    let (os, withheld) = (
        FakeOs::launched(env_naming(&rig.context)),
        rig.server.withheld.clone(),
    );
    let os: Arc<dyn ProcessOs> = Arc::new(Os(os, after.clone(), rig.context.clone(), withheld));
    let socket = (Duration::from_millis(900), Duration::from_secs(2));
    let transport = transport_with(&rig.server, peers, socket.0, socket.1);
    let registry = HerdrRegistry::with_transport(endpoint(&rig.server), transport);
    let _registry = herdr_registry::force_for_test(registry.with_reads((off_where_dialled, os)));
    let stored = stored(&rig.server, HostedState::Bound);
    let target = InputTarget::from_hosted_target_for_test(&resolved(PANE), &stored);
    let other = herdr(&target).clone();
    let _watch = watch_actions(other.clone(), move |action| {
        if let TuiInputAction::PasteBuffer(_) = action {
            armed.server.store(server, SeqCst);
            if armed.other_write {
                host_input::run_herdr(&other, &[TuiInputAction::Escape], None);
            }
            armed.armed.store(true, SeqCst);
        }
    });
    let runs = plans
        .iter()
        .map(|plan| host_input::run_herdr(herdr(&target), plan, Some(&after.token)));
    let run = runs.last().unwrap();
    (run, rig.server.sends(), target)
}

fn paste_then_enter(after: After) -> (InputRun, Vec<Value>, InputTarget) {
    run_plans(
        after,
        &[&[
            TuiInputAction::PasteBuffer("본문".into()),
            TuiInputAction::Enter,
        ]],
    )
}

fn sent(enter: bool) -> Vec<Value> {
    let paste = json!({"pane_id": PANE, "text": "\u{1b}[200~본문\u{1b}[201~"});
    let enter = enter.then(|| json!({"pane_id": PANE, "keys": ["enter"]}));
    std::iter::once(paste).chain(enter).collect()
}

fn stopped(why: HerdrGateRefusal) -> InputRun {
    let cause = StopCause::Refused(InputRefusal::Herdr(why));
    InputRun::Indeterminate {
        confirmed: 1,
        cause,
    }
}

#[test]
fn enter_readmit_transient_gap_up_to_three_times_then_keeps_indeterminate() {
    let expiry = stopped(HerdrGateRefusal::PaneUnverified { transient: true });
    for (unreadable, run) in [(1, InputRun::Applied), (3, InputRun::Applied), (4, expiry)] {
        let (got, sends, _) = paste_then_enter(after(unreadable, None, SERVER));
        let enter = run == InputRun::Applied;
        assert_eq!((got, sends), (run, sent(enter)), "{unreadable}");
    }
}

// Each forbidden refusal holds for one judgment only, so a retry past it would send the Enter;
// the late case is a judgment that succeeds only after the one-second budget.
#[test]
fn enter_readmit_forbidden_gap_witness_change_or_late_judgment_sends_no_enter() {
    use HerdrGateRefusal as Why;
    let foreign: Env = |_| env_naming(&context(&"f".repeat(32)));
    let herdr_env: Env = |own| [env_naming(own), vec!["HERDR_PANE_ID=w1-1".into()]].concat();
    let late = After {
        withhold: 1,
        ..after(1, None, SERVER)
    };
    let cases = [
        (after(0, Some(foreign), SERVER), Why::OtherNonce),
        (after(1, Some(foreign), SERVER), Why::OtherNonce),
        (
            after(0, Some(herdr_env), SERVER),
            Why::PaneUnverified { transient: false },
        ),
        (after(1, None, 8), Why::ServerChanged),
        (late, Why::PaneUnverified { transient: true }),
    ];
    for (after, why) in cases {
        let (run, sends, target) = paste_then_enter(after);
        assert_eq!((run, sends), (stopped(why), sent(false)), "{why:?}");
        assert!(herdr(&target).termination_fence().is_ok(), "{why:?}");
    }
}

// The Enter follows this plan's own latest write: another holder's write after it, or a write
// that only an earlier plan made, leaves no write to follow.
#[test]
fn enter_readmit_stands_only_on_its_own_plans_latest_write() {
    let other = After {
        other_write: true,
        ..after(1, None, 8)
    };
    let (run, sends, _) = paste_then_enter(other);
    let escape = json!({"pane_id": PANE, "keys": ["esc"]});
    let expected = [sent(false), vec![escape]].concat();
    assert_eq!(
        (run, sends),
        (stopped(HerdrGateRefusal::ServerChanged), expected)
    );
    let paste: &[_] = &[TuiInputAction::PasteBuffer("본문".into())];
    let (run, sends, _) = run_plans(after(1, None, SERVER), &[paste, &[TuiInputAction::Enter]]);
    let why = HerdrGateRefusal::PaneUnverified { transient: true };
    assert_eq!(
        (run, sends),
        (InputRun::Refused(InputRefusal::Herdr(why)), sent(false))
    );
}

// A cancel at the gap ends the wait even while every later judgment fails too.
#[test]
fn enter_readmit_cancel_during_probe_sends_no_enter() {
    for (at, unreadable) in [("gap", 1), ("env", 1), ("gap", 4)] {
        let (run, sends, target) = paste_then_enter(After {
            cancel_at: at,
            ..after(unreadable, None, SERVER)
        });
        let cancelled = InputRun::Cancelled { confirmed: 1 };
        assert_eq!((run, sends), (cancelled, sent(false)), "{at} {unreadable}");
        assert!(herdr(&target).termination_fence().is_ok(), "{at}");
    }
}
