//! An Enter refused on a transient pane gap after its paste is judged whole again, only on the
//! paste's server, at most three times; every other refusal ends the plan as before.
use super::*;
use crate::services::claude_tui::host_input::watch_actions;
use crate::services::provider::CancelToken;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicU32, AtomicUsize};

type Env = fn(&std::path::Path) -> Vec<String>;

/// Once the paste lands: provider start reads that fail, one environment read from `env`, the
/// server later dials reach, and where the turn is cancelled.
struct After(
    AtomicBool,
    AtomicUsize,
    Mutex<Option<Env>>,
    AtomicU32,
    &'static str,
    CancelToken,
);

fn after(unreadable: usize, env: Option<Env>, server: u32) -> After {
    let (unreadable, env, token) = (unreadable.into(), Mutex::new(env), CancelToken::new());
    After(false.into(), unreadable, env, server.into(), "", token)
}

impl After {
    /// Whether the plan reads the pane after the paste; a cancel set for `at` lands here.
    fn reached(&self, at: &str) -> bool {
        let armed = self.0.load(SeqCst);
        self.5.cancelled.fetch_or(armed && self.4 == at, SeqCst);
        armed
    }
}

struct Os(FakeOs, Arc<After>, PathBuf);

impl ProcessOs for Os {
    fn parent(&self, pid: u32) -> Result<u32, RestoreUnverified> {
        self.0.parent(pid)
    }
    fn start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        let failing = |n: usize| n.checked_sub(1);
        let armed = pid == PROVIDER && self.1.0.load(SeqCst);
        match armed && self.1.1.fetch_update(SeqCst, SeqCst, failing).is_ok() {
            true if self.1.reached("gap") => Err(RestoreUnverified::ProcessUnreadable),
            _ => self.0.start(pid),
        }
    }
    fn environ(&self, pid: u32) -> Result<Vec<String>, RestoreUnverified> {
        let env = self
            .1
            .reached("env")
            .then(|| self.1.2.lock().unwrap().take());
        env.flatten()
            .map_or_else(|| self.0.environ(pid), |env| Ok(env(&self.2)))
    }
    fn exec_path(&self, pid: u32) -> Option<String> {
        self.0.exec_path(pid)
    }
}

/// Pastes and presses Enter on the production chain; `after` takes hold once the paste lands.
fn paste_then_enter(after: After) -> (InputRun, Vec<Value>, InputTarget) {
    let (rig, after) = (rig(), Arc::new(after));
    let (dialled, armed) = (after.clone(), after.clone());
    let server = after.3.swap(SERVER, SeqCst);
    let peers: PeerReader = Box::new(move |_| Ok((dialled.3.load(SeqCst), start(1_000))));
    let os = FakeOs::launched(env_naming(&rig.context));
    let os: Arc<dyn ProcessOs> = Arc::new(Os(os, after.clone(), rig.context.clone()));
    let transport = transport_with(&rig.server, peers);
    let registry = HerdrRegistry::with_transport(endpoint(&rig.server), transport);
    let _registry = herdr_registry::force_for_test(registry.with_reads((off_where_dialled, os)));
    let stored = stored(&rig.server, HostedState::Bound);
    let target = InputTarget::from_hosted_target(&resolved(PANE), &stored);
    let _watch = watch_actions(herdr(&target).clone(), move |action| {
        if let TuiInputAction::PasteBuffer(_) = action {
            armed.3.store(server, SeqCst);
            armed.0.store(true, SeqCst);
        }
    });
    let plan = [
        TuiInputAction::PasteBuffer("본문".into()),
        TuiInputAction::Enter,
    ];
    let run = host_input::run_herdr(herdr(&target), &plan, Some(&after.5));
    (run, rig.server.sends(), target)
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

// Each forbidden refusal holds for one judgment only, so a retry past it would send the Enter.
#[test]
fn enter_readmit_forbidden_gap_or_witness_change_sends_no_enter() {
    use HerdrGateRefusal as Why;
    let foreign: Env = |_| env_naming(&context(&"f".repeat(32)));
    let herdr_env: Env = |own| [env_naming(own), vec!["HERDR_PANE_ID=w1-1".into()]].concat();
    let cases = [
        (after(0, Some(foreign), SERVER), Why::OtherNonce),
        (after(1, Some(foreign), SERVER), Why::OtherNonce),
        (
            after(0, Some(herdr_env), SERVER),
            Why::PaneUnverified { transient: false },
        ),
        (after(1, None, 8), Why::ServerChanged),
    ];
    for (after, why) in cases {
        let (run, sends, _) = paste_then_enter(after);
        assert_eq!((run, sends), (stopped(why), sent(false)), "{why:?}");
    }
}

#[test]
fn enter_readmit_cancel_during_probe_sends_no_enter() {
    for at in ["gap", "env"] {
        let (run, sends, target) = paste_then_enter(After {
            4: at,
            ..after(1, None, SERVER)
        });
        assert_eq!(
            (run, sends),
            (InputRun::Cancelled { confirmed: 1 }, sent(false))
        );
        assert!(herdr(&target).termination_fence().is_ok(), "{at}");
    }
}
