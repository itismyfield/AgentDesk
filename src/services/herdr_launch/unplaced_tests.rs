//! The unplaced judgment over scripted pane readings, and the in-flight registry's lifetime.
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::UNIX_EPOCH;

use super::*;
use crate::services::herdr_launch::{
    HerdrCreateOutcome, HerdrCreateRequest, HerdrLaunchCommand, RestoreResume, prepared,
};
use crate::services::session_host::EvidenceGap;

const N: &str = "0123456789abcdef0123456789abcdef";
const M: &str = "fedcba9876543210fedcba9876543210";
const K: &str = "00000000000000000000000000000abc";
const KEY: &str = "AgentDesk-claude-adk";

fn owner() -> HostedOwner {
    HostedOwner {
        provider: "claude".into(),
        discord_token_hash: "hash".into(),
        channel_id: "42".into(),
        logical_key: KEY.into(),
        owner_node: "mac-mini".into(),
        runtime_root: "/rt".into(),
    }
}

fn at(seconds: u64, millis: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds) + Duration::from_millis(millis)
}

/// Execution N's context was written at 1000s.
fn created() -> SystemTime {
    at(1_000, 0)
}

fn stamp(pid: u32) -> ProcessStamp {
    ProcessStamp {
        pid,
        start: format!("start-{pid}"),
    }
}

fn pane(id: &str, label: &str, occupant: PaneOccupant) -> ChannelPane {
    ChannelPane {
        pane_id: id.into(),
        label: label.into(),
        pristine: true,
        occupant,
    }
}

fn marked(nonce: &str) -> String {
    super::super::workspace_marker(KEY, nonce)
}

fn shell(started: SystemTime) -> PaneOccupant {
    PaneOccupant::LoneShell {
        root: stamp(10),
        root_started: started,
    }
}

/// A provider whose environment names context file `name`.
fn provider(name: &str) -> PaneOccupant {
    PaneOccupant::Provider {
        root: stamp(10),
        root_started: at(1_001, 0),
        context: PathBuf::from(format!("/rt/runtime/binding_contexts/claude/{name}.json")),
    }
}

/// Contexts by file name: N and M of this channel, `foreign` of another runtime, `broken` unreadable,
/// `elsewhere` of another channel.
fn context_of(path: &Path) -> Result<Option<BindingContext>, String> {
    let name = path.file_stem().and_then(|stem| stem.to_str()).unwrap();
    let ctx = |nonce: &str, channel: u64| BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: chrono::DateTime::<chrono::Utc>::from(created()),
        execution_nonce: nonce.into(),
        tmux_session: KEY.into(),
        channel_id: Some(channel),
        owner_runtime_root: "/rt".into(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: None,
        first_prompt_digest: None,
        source_policy: None,
    };
    match name {
        "broken" => Err("context corrupt".into()),
        "foreign" => Ok(None),
        "elsewhere" => Ok(Some(ctx(M, 7))),
        nonce => Ok(Some(ctx(nonce, 42))),
    }
}

fn judged(panes: &[ChannelPane]) -> Plan {
    judge(panes, &owner(), N, created(), &context_of)
}

fn held(plan: &Plan) -> bool {
    matches!(plan, Plan::Held(_))
}

#[test]
fn a_verified_provider_of_the_launch_is_adopted_under_its_marker_or_a_pre_marker_bare_label() {
    let marker = [pane("p1", &marked(N), provider(N))];
    assert_eq!(
        judged(&marker),
        Plan::Adopt {
            pane_id: "p1".into()
        }
    );
    let bare = [pane("p1", KEY, provider(N))];
    assert_eq!(
        judged(&bare),
        Plan::Adopt {
            pane_id: "p1".into()
        }
    );
    // A provider under another launch's marker, or another execution under the bare label, is not
    // where its own create put it: neither adopted nor closed.
    let misplaced = [
        pane("p1", &marked(M), provider(N)),
        pane("p1", &marked(N), provider(M)),
        pane("p1", &marked(K), provider(M)),
        pane("p1", KEY, provider(M)),
    ];
    for case in misplaced {
        let plan = judged(std::slice::from_ref(&case));
        assert!(held(&plan), "{case:?}: {plan:?}");
    }
}

#[test]
fn a_pane_that_cannot_be_attributed_holds_the_row() {
    let touched = ChannelPane {
        pristine: false,
        ..pane("p1", &marked(N), shell(at(1_001, 0)))
    };
    let cases = [
        pane(
            "p1",
            &marked(N),
            PaneOccupant::Unreadable("several children".into()),
        ),
        pane("p1", &marked(M), provider("broken")),
        touched,
    ];
    for case in cases {
        assert!(held(&judged(std::slice::from_ref(&case))), "{case:?}");
    }
    // A bare shell read as starting half a second before N's context may have started after it.
    let near = [pane("p1", KEY, shell(at(999, 500)))];
    assert_eq!(
        judged(&near),
        Plan::Held("pane p1: legacy_unattributed".into())
    );
    let several = [
        pane("p1", &marked(N), provider(N)),
        pane("p2", KEY, provider(N)),
    ];
    assert!(held(&judged(&several)));
    let both = [
        pane("p1", &marked(N), provider(N)),
        pane("p2", &marked(N), shell(at(1_001, 0))),
    ];
    assert!(held(&judged(&both)));
}

#[test]
fn the_untouched_shell_of_the_launch_is_removed_and_other_shells_are_left_alone() {
    let residue = [pane("p1", &marked(N), shell(at(1_001, 0)))];
    let expected = Plan::Dispose {
        pane_id: "p1".into(),
        label: marked(N),
        root: stamp(10),
    };
    assert_eq!(judged(&residue), expected);
    // An earlier execution's shell and a bare shell older than N's context are no one's launch.
    let others = [
        pane("p1", &marked(M), shell(at(1_001, 0))),
        pane("p2", KEY, shell(at(998, 900))),
    ];
    assert_eq!(judged(&others), Plan::Retire);
}

#[test]
fn unrecorded_executions_come_before_adoption_and_foreign_panes_are_ignored() {
    let panes = [
        pane("p1", &marked(N), provider(N)),
        pane("p2", &marked(M), provider(M)),
        pane("p3", &marked(M), provider("foreign")),
        pane("p4", &marked(M), provider("elsewhere")),
        pane(
            "p5",
            "AgentDesk-claude-other",
            PaneOccupant::Unreadable("not read".into()),
        ),
    ];
    assert_eq!(judged(&panes), Plan::Orphans(vec![(M.into(), "p2".into())]));
    assert_eq!(judged(&panes[2..]), Plan::Retire);
}

/// Answers E7 `Off`, and blocks its create until the test lets it go.
struct BlockedCreate {
    entered: Mutex<mpsc::Sender<()>>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl HerdrLaunchHost for BlockedCreate {
    fn mode(&self, _: &HerdrLaunchEndpoint) -> crate::config::session_hosts::EndpointMode {
        crate::config::session_hosts::EndpointMode::LegacyDedicated
    }
    fn restore_resume(&self, _: &HerdrLaunchEndpoint) -> RestoreResume {
        RestoreResume::Off {
            witness: ServerWitness::for_test(7),
        }
    }
    fn create(&self, _: &HerdrCreateRequest) -> HerdrCreateOutcome {
        self.entered.lock().unwrap().send(()).unwrap();
        let _ = self.release.lock().unwrap().recv();
        HerdrCreateOutcome::Indeterminate("reply lost".into())
    }
    fn launch_evidence(
        &self,
        _: &EvidenceProbe,
    ) -> Result<crate::db::dispatched_sessions::hosted_execution::ExpectedExecution, EvidenceGap>
    {
        Err(EvidenceGap::NoneYet)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launch_stays_in_flight_until_its_create_worker_returns_even_when_its_caller_is_gone() {
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let host: Arc<dyn HerdrLaunchHost> = Arc::new(BlockedCreate {
        entered: Mutex::new(entered_tx),
        release: Mutex::new(release_rx),
    });
    let pool = PgPool::connect_lazy("postgres://unused@localhost/unused").unwrap();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let endpoint = HerdrLaunchEndpoint {
        execution_node: "mac-mini".into(),
        config_key: "pilot".into(),
        socket_addr: "/tmp/unused.sock".into(),
        herdr_session: "adk".into(),
    };
    let launch = {
        let (nonce, endpoint) = (nonce.clone(), endpoint.clone());
        tokio::spawn(async move {
            let in_flight = LaunchInFlight::begin(&nonce);
            let command = HerdrLaunchCommand {
                cwd: PathBuf::from("/tmp"),
                command: "true".into(),
            };
            let owner = owner();
            let created =
                prepared::create(&pool, &owner, &endpoint, command, &host, None, &in_flight);
            let _ = created.await;
        })
    };
    tokio::task::spawn_blocking(move || entered.recv().unwrap())
        .await
        .unwrap();
    launch.abort();
    let _ = launch.await;
    let past = at(0, 0);
    assert_eq!(
        quiet_since(&nonce, past, Duration::ZERO),
        Err("launch in flight".into()),
        "the dropped caller must not end a create the server may still act on"
    );
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while quiet_since(&nonce, past, Duration::ZERO).is_err() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(quiet_since(&nonce, past, Duration::ZERO), Ok(()));
    assert_eq!(
        quiet_since(&nonce, past, Duration::from_secs(60)),
        Err("launch too recent".into())
    );
}
