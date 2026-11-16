//! An Enter refused on a transient pane gap after its paste is judged whole again, only on the
//! paste's server, at most three times; every other refusal ends the plan as before.
use super::*;
use crate::services::claude_tui::host_input::watch_actions;
use crate::services::provider::CancelToken;
use std::sync::atomic::{AtomicU32, AtomicUsize};

type Env = fn(&std::path::Path) -> Vec<String>;

/// Once the paste lands: provider start reads that fail, one environment read from `env`, the
/// server later dials reach, and where the turn is cancelled.
struct After {
    armed: AtomicBool,
    unreadable: AtomicUsize,
    env: Mutex<Option<Env>>,
    server: AtomicU32,
    cancel_at: &'static str,
    token: CancelToken,
}

fn after(unreadable: usize, env: Option<Env>, server: u32) -> After {
    let (armed, unreadable, env) = (false.into(), unreadable.into(), Mutex::new(env));
    let (server, cancel_at, token) = (server.into(), "", CancelToken::new());
    After {
        armed,
        unreadable,
        env,
        server,
        cancel_at,
        token,
    }
}

struct Os(FakeOs, Arc<After>, PathBuf);

impl ProcessOs for Os {
    fn parent(&self, pid: u32) -> Result<u32, RestoreUnverified> {
        self.0.parent(pid)
    }
    fn start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        let (after, left) = (&self.1, |n: usize| n.checked_sub(1));
        if !after.armed.load(Ordering::SeqCst)
            || pid != PROVIDER
            || after
                .unreadable
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, left)
                .is_err()
        {
            return self.0.start(pid);
        }
        after
            .token
            .cancelled
            .fetch_or(after.cancel_at == "gap", Ordering::SeqCst);
        Err(RestoreUnverified::ProcessUnreadable)
    }
    fn environ(&self, pid: u32) -> Result<Vec<String>, RestoreUnverified> {
        let after = &self.1;
        if !after.armed.load(Ordering::SeqCst) {
            return self.0.environ(pid);
        }
        after
            .token
            .cancelled
            .fetch_or(after.cancel_at == "env", Ordering::SeqCst);
        let env = after.env.lock().unwrap().take();
        env.map_or_else(|| self.0.environ(pid), |env| Ok(env(&self.2)))
    }
    fn exec_path(&self, pid: u32) -> Option<String> {
        self.0.exec_path(pid)
    }
}

/// Pastes and presses Enter on the production chain; `after` takes hold once the paste lands.
fn paste_then_enter(after: After) -> (InputRun, Vec<Value>, InputTarget) {
    let (rig, after) = (rig(), Arc::new(after));
    let (dialled, armed, server) = (
        after.clone(),
        after.clone(),
        after.server.swap(SERVER, Ordering::SeqCst),
    );
    let peers: PeerReader =
        Box::new(move |_| Ok((dialled.server.load(Ordering::SeqCst), start(1_000))));
    let os = Os(
        FakeOs::launched(env_naming(&rig.context)),
        after.clone(),
        rig.context.clone(),
    );
    let transport = transport_with(&rig.server, peers);
    let registry = HerdrRegistry::with_transport(endpoint(&rig.server), transport);
    let reads = (
        off_where_dialled as Restore,
        Arc::new(os) as Arc<dyn ProcessOs>,
    );
    let _registry = herdr_registry::force_for_test(registry.with_reads(reads));
    let stored = stored(&rig.server, HostedState::Bound);
    let target = InputTarget::from_hosted_target(&resolved(PANE), &stored);
    let _watch = watch_actions(herdr(&target).clone(), move |action| {
        if let TuiInputAction::PasteBuffer(_) = action {
            armed.server.store(server, Ordering::SeqCst);
            armed.armed.store(true, Ordering::SeqCst);
        }
    });
    let plan = [
        TuiInputAction::PasteBuffer("본문".into()),
        TuiInputAction::Enter,
    ];
    let run = host_input::run_herdr(herdr(&target), &plan, Some(&after.token));
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
    let cases = [(1, InputRun::Applied), (3, InputRun::Applied), (4, expiry)];
    for (unreadable, run) in cases {
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
    for cancel_at in ["gap", "env"] {
        let after = After {
            cancel_at,
            ..after(1, None, SERVER)
        };
        let (run, sends, target) = paste_then_enter(after);
        assert_eq!(
            (run, sends),
            (InputRun::Cancelled { confirmed: 1 }, sent(false))
        );
        assert!(herdr(&target).termination_fence().is_ok(), "{cancel_at}");
    }
}
