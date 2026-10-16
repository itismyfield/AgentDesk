//! A runtime's delegated homes end at the committed restart's exit, after SIGTERM's save and before
//! its barrier, and at a backend exit before its lease drops; a cancelled promotion keeps them.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use super::super::gateway_lease_recovery::{TerminalProof, terminal_proof};
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::cluster::channel_home;
use crate::services::cluster::channel_home_boot::{
    self, Boot, BootPort, ChannelWriters, StartWriter,
};
use crate::services::cluster::home_supervisor::{HomeLifecycle, Phase, Settled, StopReason};
use crate::services::tui_o::cutover::{self, test_override};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{ShadowProvider, binding_reader::source_id_for};
use crate::services::tui_o::writer::host::{self, HostParts, ManagedWriterHandle, Readiness};

/// Writers started for `channels` under a held gate, each in this root; a channel without a
/// source panics its host at start, so its end is never confirmed.
pub(in crate::services::discord::runtime_bootstrap) struct Hosted {
    pub(in crate::services::discord::runtime_bootstrap) root: tempfile::TempDir,
    pub(in crate::services::discord::runtime_bootstrap) gate: Arc<OwnershipGate>,
    pub(in crate::services::discord::runtime_bootstrap) ready: Arc<Readiness>,
    pub(in crate::services::discord::runtime_bootstrap) handles: Vec<ManagedWriterHandle>,
}

/// Needs the channels forced as candidates on this thread.
pub(in crate::services::discord::runtime_bootstrap) fn hosted(
    channels: &[u64],
    sourceless: &[u64],
) -> Hosted {
    let root = tempfile::tempdir().unwrap();
    let source = |channel: &u64| {
        let transcript = root.path().join(format!("{channel}.jsonl"));
        std::fs::write(&transcript, b"").unwrap();
        (
            *channel,
            source_id_for(&format!("s{channel}"), &transcript).unwrap(),
        )
    };
    let io = host::test_io::TestHost::new(channels.iter().map(source).collect::<Vec<_>>());
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let ready: Arc<Readiness> = Arc::default();
    let (runtime_root, readiness) = (root.path().to_path_buf(), Arc::clone(&ready));
    let held = Arc::clone(&gate);
    let parts = move || HostParts {
        io,
        runtime_root: Some(runtime_root),
        gate: held,
        readiness,
    };
    let all = |owned: &(u64, _, _)| channels.contains(&owned.0) || sourceless.contains(&owned.0);
    let owned = cutover::boot_ownership().into_iter().filter(all).collect();
    let handles = host::start_managed(ShadowProvider::Claude, true, owned, parts);
    Hosted {
        root,
        gate,
        ready,
        handles,
    }
}

pub(in crate::services::discord::runtime_bootstrap) async fn eventually(
    seconds: u64,
    mut done: impl FnMut() -> bool,
) -> bool {
    for _ in 0..seconds * 50 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    done()
}

fn running(homes: &ChannelWriters, channel: u64) -> bool {
    matches!(homes.phase(channel), Some(Phase::Running(_)))
}

/// A boot whose rows read nothing: each candidate is watched without a gate, over a pool that
/// never connects, so its watch only ever fails its read.
fn candidate_boot(
    candidates: Vec<u64>,
    writers: Arc<ChannelWriters>,
) -> Option<Boot<impl Fn(u64) -> BootPort>> {
    let pool =
        sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://agentdesk@127.0.0.1:1/none");
    let reset: channel_home_boot::LegacyReset = Arc::new(|| Box::pin(std::future::ready(Ok(()))));
    Some(Boot {
        provider: "claude".into(),
        local: "mini".into(),
        pool: pool.unwrap(),
        rows: Box::pin(async { Ok(Vec::new()) }),
        candidates,
        port: move |channel| BootPort::reading(channel, Arc::default(), Arc::clone(&reset)),
        writers: Some(writers),
    })
}

/// A late stop of an earlier start's homes ends only what that start began: the watch the same
/// channel's later start registered keeps running, and so does its writer.
#[tokio::test]
async fn a_late_old_cleanup_leaves_the_successor_running() {
    const OLD: u64 = 9_300_000_000_000_051;
    const NEW: u64 = 9_300_000_000_000_052;
    let _candidates = test_override::force_candidates(&[(OLD, ClaudeTui), (NEW, ClaudeTui)]);
    let homes = |hosted: Hosted| {
        let homes = ChannelWriters::new(Arc::default(), Arc::clone(&hosted.ready)).delegated();
        homes.arm(None, hosted.handles);
        (Arc::new(homes), hosted.root, hosted.ready)
    };
    let (old, _old_root, _) = homes(hosted(&[OLD], &[]));
    let (new, _new_root, new_ready) = homes(hosted(&[NEW], &[]));
    let channel = NEW.to_string();
    channel_home_boot::start(Some(true), || candidate_boot(vec![NEW], Arc::clone(&old))).await;
    // The same channel's later start ends the earlier watch and registers its own.
    channel_home_boot::start(Some(true), || candidate_boot(vec![NEW], Arc::clone(&new))).await;
    assert!(eventually(10, || running(&old, OLD) && running(&new, NEW)).await);
    assert!(channel_home_boot::running(&channel).0);

    assert_eq!(
        old.stop_and_join(StopReason::Sigterm).await,
        Settled::Joined
    );
    assert_eq!(old.phase(OLD), None, "the old start's own writer ended");
    assert!(
        channel_home_boot::running(&channel).0,
        "the successor's watch survives a late old cleanup"
    );
    assert!(
        running(&new, NEW) && new_ready.is_hosted(NEW),
        "its writer still runs"
    );

    assert_eq!(
        new.stop_and_join(StopReason::Sigterm).await,
        Settled::Joined
    );
    assert_eq!(channel_home_boot::running(&channel), (false, None));
    assert!(!new_ready.is_hosted(NEW));
}

/// A writer whose end is unconfirmed is `Stuck`, not joined, and its channel stays blocked on
/// every later stop; the confirmed one beside it still ends.
#[tokio::test]
async fn a_stuck_writer_stays_stuck_and_the_commit_still_exits() {
    const ENDS: u64 = 9_300_000_000_000_061;
    const STUCK: u64 = 9_300_000_000_000_062;
    let _candidates = test_override::force_candidates(&[(ENDS, ClaudeTui), (STUCK, ClaudeTui)]);
    let hosted = hosted(&[ENDS], &[STUCK]);
    let homes = ChannelWriters::new(Arc::default(), Arc::clone(&hosted.ready)).delegated();
    homes.arm(None, hosted.handles);
    assert!(eventually(10, || running(&homes, ENDS) && running(&homes, STUCK)).await);

    for stop in 0..2 {
        let settled = homes.stop_and_join(StopReason::CommittedRestart).await;
        assert!(
            matches!(settled, Settled::Stuck(_)),
            "stop {stop}: {settled:?}"
        );
        assert!(matches!(homes.phase(STUCK), Some(Phase::Blocked(..))));
        assert_eq!(homes.phase(ENDS), None);
        assert!(!hosted.ready.is_hosted(ENDS));
    }
    // The committed path logs it and still exits: the process child below shows the exit.
    let (status, evidence, proof) = committed_child(true);
    assert!(
        status.success(),
        "a stuck home never holds the committed exit: {status:?}"
    );
    assert_eq!(proof, TerminalProof::Proven);
    assert!(evidence.contains("claude None false"), "{evidence}");
    assert!(evidence.contains("stuck Some(Blocked("), "{evidence}");
}

/// Ending the homes ends their watches first, so a candidate watch never starts another writer.
#[tokio::test]
async fn shutdown_ends_watches_before_writers_pg() {
    const CH: u64 = 9_300_000_000_000_071;
    let _candidates = test_override::force_candidates(&[(CH, ClaudeTui)]);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let hosted = hosted(&[CH], &[]);
    let gate = Arc::clone(&hosted.gate);
    let homes = Arc::new(ChannelWriters::new(gate, Arc::clone(&hosted.ready)));
    let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let restart: StartWriter = {
        let (starts, ready) = (Arc::clone(&starts), Arc::clone(&hosted.ready));
        let root = hosted.root.path().to_path_buf();
        Arc::new(move |channel: u64| {
            starts.fetch_add(1, Ordering::SeqCst);
            restarted(channel, root.clone(), Arc::clone(&ready))
        })
    };
    homes.arm(Some(restart), hosted.handles);
    let mut boot = candidate_boot(vec![CH], Arc::clone(&homes)).unwrap();
    boot.pool = pool.clone();
    channel_home_boot::start(Some(true), || Some(boot)).await;
    let channel = CH.to_string();
    assert!(
        eventually(10, || running(&homes, CH)
            && channel_home_boot::rounds(CH) >= 1)
        .await
    );

    assert_eq!(
        homes.stop_and_join(StopReason::Sigterm).await,
        Settled::Joined
    );
    assert!(!channel_home_boot::running(&channel).0, "the watch ended");
    tokio::time::sleep(channel_home::RENEW_EVERY * 2).await;
    assert_eq!(homes.phase(CH), None, "no writer started after the stop");
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    pool.close().await;
    db.drop().await;
}

fn restarted(
    channel: u64,
    root: PathBuf,
    readiness: Arc<Readiness>,
) -> Option<ManagedWriterHandle> {
    let transcript = root.join(format!("{channel}.jsonl"));
    let source = source_id_for(&format!("s{channel}"), &transcript).unwrap();
    let io = host::test_io::TestHost::new([(channel, source)]);
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let parts = move || HostParts {
        io,
        runtime_root: Some(root),
        gate,
        readiness,
    };
    let owned = cutover::boot_ownership().into_iter();
    let owned = owned.filter(|(owned, ..)| *owned == channel).collect();
    host::start_managed(ShadowProvider::Claude, true, owned, parts).pop()
}

/// A backend exit closes O admission, then joins its writers while the lease still holds.
#[tokio::test]
async fn backend_exit_joins_writers_before_the_lease_drops() {
    const CH: u64 = 9_300_000_000_000_081;
    let _candidates = test_override::force_candidates(&[(CH, ClaudeTui)]);
    let provider = ProviderKind::Unsupported("s3c-backend-exit".into());
    let hosted = hosted(&[CH], &[]);
    let homes = super::super::o_writer_host::home_writers(&provider, false);
    homes.arm(None, hosted.handles);
    assert!(eventually(10, || running(&homes, CH)).await);

    struct LeaseDrop(Box<dyn FnOnce() + Send>);
    impl Drop for LeaseDrop {
        fn drop(&mut self) {
            (std::mem::replace(&mut self.0, Box::new(|| {})))();
        }
    }
    let seen = Arc::new(std::sync::Mutex::new(None));
    let probe = {
        let (seen, homes, ready) = (
            Arc::clone(&seen),
            Arc::clone(&homes),
            Arc::clone(&hosted.ready),
        );
        LeaseDrop(Box::new(move || {
            *seen.lock().unwrap() = Some((homes.phase(CH), ready.is_hosted(CH)));
        }))
    };
    let lease = tokio::spawn(async move {
        let _probe = probe;
        std::future::pending::<()>().await;
    });
    let gate = crate::services::tui_o::ownership::gate(provider.as_str());
    gate.acquired();
    let backend = tokio::spawn(async { Ok::<(), &str>(()) });
    super::finish_gateway_backend(backend, &provider, None, Some(lease), None, async {
        assert_eq!(gate.admit(|epoch| epoch), None, "admission closed first");
        assert_eq!(homes.phase(CH), None, "joined before the diagnostic");
    })
    .await;
    assert_eq!(
        *seen.lock().unwrap(),
        Some((None, false)),
        "joined before the lease dropped"
    );
}

/// With the switch off no runtime builds a lifecycle, so every boundary ends nothing new.
#[tokio::test]
async fn switch_off_shutdown_builds_no_lifecycle() {
    let lazy =
        sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://agentdesk@127.0.0.1:1/none");
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(lazy.unwrap()));
    for role in [HomeRole::Gateway, HomeRole::Standby, HomeRole::RestWorker] {
        let off = HomeSettings::default();
        assert!(
            start_channel_homes(&shared, &ProviderKind::Claude, role, &off)
                .await
                .is_empty()
        );
    }
    let delegated =
        super::super::o_writer_host::spawn_delegated(&shared, &ProviderKind::Claude, None);
    assert!(delegated.is_empty());
    assert!(super::super::o_writer_host::lifecycles(None).is_empty());
}

/// One SIGTERM handler per runtime that keeps its shutdown slot, none where the slot is returned.
#[test]
fn every_slot_holding_runtime_registers_one_sigterm_handler() {
    const BOOT: &str = include_str!("../runtime_bootstrap.rs");
    let body = BOOT.split_once("pub(crate) async fn run_bot(").unwrap().1;
    let body = body.split_once("\n}\n").unwrap().0;
    let handler = "run_bot_spawn_sigterm_handler(&shared, provider.clone());";
    let branch = |from: &str, to: &str| {
        let rest = body.split_once(from).expect(from).1;
        rest.split_once(to).expect(to).0.matches(handler).count()
    };
    assert_eq!(body.matches(handler).count(), 3);
    assert_eq!(branch("if !modules.gateway {", "return;"), 1, "REST worker");
    assert_eq!(
        branch("GatewayLeaseOutcome::Standby => {", "return;"),
        1,
        "standby"
    );
    // The lease match's last arm closes it; everything after belongs to the gateway branch.
    let gateway = "GatewayLeaseOutcome::Failed => {";
    assert_eq!(
        branch(gateway, "run_bot_start_gateway_runtime("),
        1,
        "gateway"
    );
    let utility = "should_skip_agent_runtime_launch(token) {";
    assert_eq!(branch(utility, "return;"), 0, "utility");
    assert_eq!(
        branch("!gateway_outcome.starts_provider_runtime() {", "return;"),
        0,
        "lease failure"
    );
    assert!(!include_str!("gateway_runtime.rs").contains("run_bot_spawn_sigterm_handler"));
}

/// A standby's SIGTERM, after its two saves and before its barrier slot, ends its watch and then
/// joins its delegated writer through the production boot of both.
#[cfg(unix)]
#[tokio::test]
async fn standby_sigterm_joins_its_homes_before_the_barrier_pg() {
    use crate::db::o_channel_homes::{self, HomeWrite};
    use crate::services::tui_o::channel_policy::Adoption;
    const HELD: u64 = 9_300_000_000_000_091;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = HELD.to_string();
    let applied = |write: Result<HomeWrite<o_channel_homes::ChannelHome>, _>| match write {
        Ok(HomeWrite::Applied(row)) => row,
        other => panic!("{other:?}"),
    };
    let row = applied(o_channel_homes::delegate(&pool, &channel, "claude", "gw", "mini").await);
    let row = applied(o_channel_homes::finish_release(&pool, &channel, "gw", row.epoch).await);
    let epoch = applied(o_channel_homes::adopt(&pool, &channel, "mini", row.epoch).await).epoch;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let _hosts = crate::config::session_hosts::force_for_test(Some("mini"), &[(HELD, "mini")]);
    let mock = crate::services::discord::shared_state::test_rest::recording_mock(1, HELD);
    let (_log, _rest) = mock.await;
    let on = HomeSettings {
        switch: Some(true),
        instance_id: Some("mini".into()),
    };
    let provider = ProviderKind::Claude;
    start_channel_homes(&shared, &provider, HomeRole::Standby, &on).await;
    assert!(
        eventually(10, || channel_home_boot::running(&channel)
            == (true, Some(epoch)))
        .await
    );
    let _standby = test_override::force_standby(&[(HELD, ClaudeTui)], "gw", Adoption::Committed);
    let started = super::super::o_writer_host::spawn_delegated(&shared, &provider, None);
    assert_eq!(started, vec![HELD]);
    let homes = super::super::o_writer_host::home_writers(&provider, true);
    assert!(eventually(10, || running(&homes, HELD)).await);

    shared.restart.shutdown_remaining.store(2, Ordering::SeqCst);
    let saved = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (signal, received) = tokio::sync::oneshot::channel();
    let at_final = Arc::clone(&saved);
    super::SIGTERM_FOR_TEST.with(|slot| {
        *slot.borrow_mut() = Some(super::SigtermForTest {
            signal: received,
            after_initial: Box::new(|| {}),
            before_final: Box::new(move || at_final.store(true, Ordering::SeqCst)),
        })
    });
    let handler = run_bot_spawn_sigterm_handler(&shared, provider.clone());
    // What the runtime looked like at the first moment its barrier slot was gone.
    let probe = {
        let (shared, homes, saved, channel) =
            (shared.clone(), Arc::clone(&homes), saved, channel.clone());
        tokio::spawn(async move {
            while shared.restart.shutdown_remaining.load(Ordering::SeqCst) == 2 {
                tokio::task::yield_now().await;
            }
            let watching = channel_home_boot::running(&channel).0;
            (saved.load(Ordering::SeqCst), watching, homes.phase(HELD))
        })
    };
    signal.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(30), handler)
        .await
        .unwrap()
        .unwrap();
    let at_barrier = probe.await.unwrap();
    assert_eq!(
        at_barrier,
        (true, false, None),
        "saved, watch ended, writer joined"
    );
    assert_eq!(shared.restart.shutdown_remaining.load(Ordering::SeqCst), 1);
    assert!(
        !host::process_readiness().is_hosted(HELD),
        "its claim was released"
    );
    channel_home::unregister(&channel);
    pool.close().await;
    db.drop().await;
}

const CHILD: &str = "ADK_S3C_COMMIT_CHILD";
const CHILD_STUCK: &str = "ADK_S3C_COMMIT_CHILD_STUCK";
const CHILD_EVIDENCE: &str = "ADK_S3C_COMMIT_EVIDENCE";
const COMMIT_NONCE: &str = "s3c-committed-restart";

/// Runs the committed-restart child in a fresh root and returns its exit, what it saw at exit and
/// the sentinel's proof.
fn committed_child(stuck: bool) -> (std::process::ExitStatus, String, TerminalProof) {
    let root = tempfile::tempdir().unwrap();
    let marker = format!("nonce={COMMIT_NONCE}\nreason=s3c\n");
    std::fs::write(root.path().join("restart_pending"), marker).unwrap();
    let evidence = root.path().join("s3c-exit-evidence");
    let name = concat!(
        "services::discord::runtime_bootstrap::shutdown::home_lifecycle_tests::",
        "committed_restart_child"
    );
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child.args([
        "--exact",
        name,
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    child.env(CHILD, "1").env("AGENTDESK_ROOT_DIR", root.path());
    child.env(CHILD_EVIDENCE, &evidence).env_remove(CHILD_STUCK);
    if stuck {
        child.env(CHILD_STUCK, "1");
    }
    let mut child = child.stdout(std::process::Stdio::null()).spawn().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("the committed restart child never exited");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let seen = std::fs::read_to_string(&evidence).unwrap_or_default();
    (status, seen, terminal_proof(root.path(), COMMIT_NONCE))
}

/// Two providers' homes in one process: only the final provider's poller commits, and both
/// providers' writers have ended when the process exits.
#[test]
fn committed_restart_joins_every_provider_writer_before_exit() {
    let (status, evidence, proof) = committed_child(false);
    assert!(
        status.success(),
        "the committed restart exits 0: {status:?}"
    );
    assert_eq!(proof, TerminalProof::Proven, "its sentinel is proven");
    assert_eq!(evidence, "claude None false\ncodex None false", "at exit");
}

type AtExit = (
    PathBuf,
    Vec<(String, Arc<ChannelWriters>, u64, Arc<Readiness>)>,
);

static AT_EXIT: OnceLock<AtExit> = OnceLock::new();

/// Records each home's writer phase and claim as the process exits.
extern "C" fn record_at_exit() {
    let Some((path, homes)) = AT_EXIT.get() else {
        return;
    };
    let seen =
        |(name, homes, channel, ready): &(String, Arc<ChannelWriters>, u64, Arc<Readiness>)| {
            format!(
                "{name} {:?} {}",
                homes.phase(*channel),
                ready.is_hosted(*channel)
            )
        };
    let _ = std::fs::write(path, homes.iter().map(seen).collect::<Vec<_>>().join("\n"));
}

/// The child: a Claude and a Codex runtime's homes, each with a running writer, then the Claude
/// poller as the final provider meets the restart marker the parent wrote.
#[test]
#[ignore = "run in its own process by committed_restart_joins_every_provider_writer_before_exit"]
fn committed_restart_child() {
    if std::env::var(CHILD).is_err() {
        return;
    }
    const CLAUDE: u64 = 9_300_000_000_000_101;
    const CODEX: u64 = 9_300_000_000_000_102;
    const STUCK: u64 = 9_300_000_000_000_103;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let all = [(CLAUDE, ClaudeTui), (CODEX, ClaudeTui), (STUCK, ClaudeTui)];
        let _candidates = test_override::force_candidates(&all);
        let mut homes = Vec::new();
        let mut roots = Vec::new();
        let stuck = std::env::var(CHILD_STUCK).is_ok();
        let providers = [
            ("claude", CLAUDE, false),
            ("codex", CODEX, false),
            ("stuck", STUCK, true),
        ];
        for (name, channel, sourceless) in providers {
            if sourceless && !stuck {
                continue;
            }
            let provider = ProviderKind::from_str_or_unsupported(name);
            let hosted = if sourceless {
                hosted(&[], &[channel])
            } else {
                hosted(&[channel], &[])
            };
            let lifecycle = super::super::o_writer_host::home_writers(&provider, false);
            lifecycle.arm(None, hosted.handles);
            homes.push((name.to_owned(), lifecycle, channel, hosted.ready));
            roots.push(hosted.root);
        }
        let up = |homes: &Vec<(String, Arc<ChannelWriters>, u64, Arc<Readiness>)>| {
            homes
                .iter()
                .all(|(_, homes, channel, _)| running(homes, *channel))
        };
        assert!(eventually(10, || up(&homes)).await, "every writer runs");
        let evidence = PathBuf::from(std::env::var(CHILD_EVIDENCE).unwrap());
        assert!(AT_EXIT.set((evidence, homes)).is_ok());
        // SAFETY: registers a plain `extern "C"` callback that only reads a set static.
        assert_eq!(unsafe { libc::atexit(record_at_exit) }, 0);
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared.restart.shutdown_remaining.store(1, Ordering::SeqCst);
        super::super::spawns::run_bot_spawn_deferred_restart_poller(&shared, &ProviderKind::Claude);
        tokio::time::sleep(Duration::from_secs(90)).await;
        drop(roots);
        panic!("the committed restart never exited");
    });
}

const SIGTERM_CHILD: &str = "ADK_S3C_SIGTERM_CHILD";
const SIGTERM_READY: &str = "ADK_S3C_SIGTERM_READY";

/// An OS SIGTERM, not the test source, reaches the production handler: the process exits 0 through
/// its barrier only after the runtime's writer was joined and its claim released.
#[cfg(unix)]
#[test]
fn an_os_sigterm_joins_the_writer_before_exit() {
    let root = tempfile::tempdir().unwrap();
    let (ready, evidence) = (root.path().join("ready"), root.path().join("evidence"));
    let name = concat!(
        "services::discord::runtime_bootstrap::shutdown::home_lifecycle_tests::",
        "os_sigterm_child"
    );
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child.args([
        "--exact",
        name,
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    child
        .env(SIGTERM_CHILD, "1")
        .env("AGENTDESK_ROOT_DIR", root.path());
    child
        .env(CHILD_EVIDENCE, &evidence)
        .env(SIGTERM_READY, &ready);
    let mut child = child.stdout(std::process::Stdio::null()).spawn().unwrap();
    let wait = |child: &mut std::process::Child,
                done: &dyn Fn(&mut std::process::Child) -> bool| {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !done(child) {
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                panic!("the SIGTERM child timed out");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    wait(&mut child, &|child| {
        assert!(child.try_wait().unwrap().is_none(), "exited before SIGTERM");
        ready.exists()
    });
    let pid = libc::pid_t::try_from(child.id()).unwrap();
    // SAFETY: signals only the child this test spawned and still holds.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    wait(&mut child, &|child| child.try_wait().unwrap().is_some());
    let status = child.wait().unwrap();
    assert_eq!(
        status.code(),
        Some(0),
        "the handler's exit, not the signal's: {status:?}"
    );
    let seen = std::fs::read_to_string(&evidence).unwrap_or_default();
    assert_eq!(seen, "claude None false", "at exit");
}

/// The child: a Claude runtime's running writer and its production SIGTERM handler over the OS
/// signal, as the last runtime in the shutdown barrier.
#[cfg(unix)]
#[test]
#[ignore = "run in its own process by an_os_sigterm_joins_the_writer_before_exit"]
fn os_sigterm_child() {
    if std::env::var(SIGTERM_CHILD).is_err() {
        return;
    }
    const CLAUDE: u64 = 9_300_000_000_000_111;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let _candidates = test_override::force_candidates(&[(CLAUDE, ClaudeTui)]);
        let provider = ProviderKind::Claude;
        let hosted = hosted(&[CLAUDE], &[]);
        let homes = super::super::o_writer_host::home_writers(&provider, false);
        homes.arm(None, hosted.handles);
        assert!(eventually(10, || running(&homes, CLAUDE)).await);
        let evidence = PathBuf::from(std::env::var(CHILD_EVIDENCE).unwrap());
        let recorded = vec![("claude".to_owned(), homes, CLAUDE, hosted.ready)];
        assert!(AT_EXIT.set((evidence, recorded)).is_ok());
        // SAFETY: registers a plain `extern "C"` callback that only reads a set static.
        assert_eq!(unsafe { libc::atexit(record_at_exit) }, 0);
        let shared = crate::services::discord::make_shared_data_for_tests();
        shared.restart.shutdown_remaining.store(1, Ordering::SeqCst);
        let _handler = run_bot_spawn_sigterm_handler(&shared, provider);
        // The handler task installs its OS listener on its first poll.
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::write(std::env::var(SIGTERM_READY).unwrap(), b"").unwrap();
        tokio::time::sleep(Duration::from_secs(90)).await;
        drop(hosted.root);
        panic!("the SIGTERM never ended the process");
    });
}
