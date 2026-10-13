//! One process-wide executor for the snapshot's tmux observations. A slot is held from admission
//! until its child is reaped, so a timed-out or dropped request never frees capacity early.

use std::panic::AssertUnwindSafe;
use std::process::{ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::FutureExt;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;
use tokio::sync::{Semaphore, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::TmuxObservationBudget;
use crate::services::discord::health::mailbox::MailboxHealthSnapshot;
use crate::services::discord::host_liveness;
use crate::services::platform::tmux::health_probe::{
    self as tmux_probe, HealthTmuxAnswer, HealthTmuxQuery, HealthTmuxRefusal,
};

const HEALTH_TMUX_MAX_INFLIGHT: usize = 4;
const DETAIL_PID_BUDGET: Duration = Duration::from_millis(250);
const OUTPUT_LIMIT: usize = 4096;
const REAP_WARN_AFTER: Duration = Duration::from_millis(250);
const REAP_RETRY: Duration = Duration::from_millis(20);

static SHARED: LazyLock<HealthProbeRuntime> =
    LazyLock::new(|| HealthProbeRuntime::new(HEALTH_TMUX_MAX_INFLIGHT));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbeFault {
    Saturated,
    Expired,
    Cancelled,
    PathUnready,
    Refused,
    Spawn,
    Io,
    Overflow,
    Panicked,
    OwnerLost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbeOutcome {
    Answered(HealthTmuxAnswer),
    Unwitnessed(ProbeFault),
}

#[derive(Clone)]
pub(super) struct HealthProbeRuntime {
    inner: Arc<Inner>,
}

struct Inner {
    slots: Semaphore,
    stats: ProbeStats,
    #[cfg(test)]
    seams: tests::Seams,
}

#[derive(Default)]
struct ProbeStats {
    admitted: AtomicUsize,
    saturated: AtomicUsize,
    spawned: AtomicUsize,
    workers: AtomicUsize,
    children: AtomicUsize,
    reaping: AtomicUsize,
}

/// A slot only `release` returns; dropping it unreleased leaves the slot spent.
struct SlotLease {
    inner: Arc<Inner>,
}

struct ProbeOwner {
    lease: SlotLease,
    query: HealthTmuxQuery,
    session: String,
    deadline: Instant,
    cancel: CancellationToken,
}

impl HealthProbeRuntime {
    pub(super) fn shared() -> Self {
        SHARED.clone()
    }

    fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                slots: Semaphore::new(limit),
                stats: ProbeStats::default(),
                #[cfg(test)]
                seams: Default::default(),
            }),
        }
    }

    /// Admits without queueing; the request only waits, the owner task alone holds the child.
    async fn observe(
        &self,
        query: HealthTmuxQuery,
        session: &str,
        budget: Duration,
    ) -> ProbeOutcome {
        #[cfg(test)]
        if let Some(outcome) = self.inner.seams.scripted(query) {
            return outcome;
        }
        let deadline = Instant::now() + budget;
        let Some(lease) = SlotLease::try_acquire(&self.inner) else {
            self.inner.stats.saturated.fetch_add(1, Ordering::Relaxed);
            return ProbeOutcome::Unwitnessed(ProbeFault::Saturated);
        };
        let cancel = CancellationToken::new();
        let _request = cancel.clone().drop_guard();
        let (sender, receiver) = oneshot::channel();
        self.inner.stats.workers.fetch_add(1, Ordering::Relaxed);
        let session = session.to_owned();
        let owner = ProbeOwner {
            lease,
            query,
            session,
            deadline,
            cancel,
        };
        tokio::spawn(owner.run(sender));
        match tokio::time::timeout_at(deadline, receiver).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => ProbeOutcome::Unwitnessed(ProbeFault::OwnerLost),
            Err(_) => ProbeOutcome::Unwitnessed(ProbeFault::Expired),
        }
    }
}

impl SlotLease {
    fn try_acquire(inner: &Arc<Inner>) -> Option<Self> {
        inner.slots.try_acquire().ok()?.forget();
        inner.stats.admitted.fetch_add(1, Ordering::Relaxed);
        Some(Self {
            inner: Arc::clone(inner),
        })
    }

    fn release(self) {
        self.inner.slots.add_permits(1);
    }
}

impl ProbeOwner {
    async fn run(self, sender: oneshot::Sender<ProbeOutcome>) {
        let inner = Arc::clone(&self.lease.inner);
        match self.spawn_child() {
            Err(fault) => {
                let _ = sender.send(ProbeOutcome::Unwitnessed(fault));
            }
            Ok(mut child) => {
                inner.stats.spawned.fetch_add(1, Ordering::Relaxed);
                inner.stats.children.fetch_add(1, Ordering::Relaxed);
                let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
                let collected =
                    AssertUnwindSafe(self.collect(&mut child, &mut stdout, &mut stderr))
                        .catch_unwind()
                        .await;
                let exited = matches!(collected, Ok(Ok(_)));
                let outcome = match collected {
                    Ok(Ok(status)) => match self.interrupted() {
                        Some(fault) => ProbeOutcome::Unwitnessed(fault),
                        None => ProbeOutcome::Answered(tmux_probe::classify_health_tmux_output(
                            self.query,
                            &Output {
                                status,
                                stdout,
                                stderr,
                            },
                        )),
                    },
                    Ok(Err(fault)) => ProbeOutcome::Unwitnessed(fault),
                    Err(_) => ProbeOutcome::Unwitnessed(ProbeFault::Panicked),
                };
                let _ = sender.send(outcome);
                if !exited {
                    inner.reap(&mut child, &self.session).await;
                }
                inner.stats.children.fetch_sub(1, Ordering::Relaxed);
            }
        }
        self.lease.release();
        inner.stats.workers.fetch_sub(1, Ordering::Relaxed);
    }

    fn interrupted(&self) -> Option<ProbeFault> {
        if self.cancel.is_cancelled() {
            Some(ProbeFault::Cancelled)
        } else if Instant::now() >= self.deadline {
            Some(ProbeFault::Expired)
        } else {
            None
        }
    }

    /// Checked on entry and again right before spawn, so a late admission never creates a child.
    fn spawn_child(&self) -> Result<Child, ProbeFault> {
        if let Some(fault) = self.interrupted() {
            return Err(fault);
        }
        let command = self.prepare().map_err(|refusal| match refusal {
            HealthTmuxRefusal::RuntimePathUnready => ProbeFault::PathUnready,
            HealthTmuxRefusal::BlankSession | HealthTmuxRefusal::Unsupported => ProbeFault::Refused,
        })?;
        let mut command = tokio::process::Command::from(command);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(fault) = self.interrupted() {
            return Err(fault);
        }
        command.spawn().map_err(|_| ProbeFault::Spawn)
    }

    fn prepare(&self) -> Result<std::process::Command, HealthTmuxRefusal> {
        #[cfg(test)]
        if let Some(prepare) = self.lease.inner.seams.prepare() {
            return tmux_probe::health_tmux_command_using(self.query, &self.session, prepare);
        }
        tmux_probe::health_tmux_command(self.query, &self.session)
    }

    /// Reads both pipes while waiting, so neither a full pipe nor an early exit stalls the other.
    async fn collect(
        &self,
        child: &mut Child,
        stdout: &mut Vec<u8>,
        stderr: &mut Vec<u8>,
    ) -> Result<ExitStatus, ProbeFault> {
        let (stdout_pipe, stderr_pipe) = (child.stdout.take(), child.stderr.take());
        let run = async {
            let wait = async { child.wait().await.map_err(|_| ProbeFault::Io) };
            let (_, _, status) = tokio::try_join!(
                read_capped(stdout_pipe, stdout),
                read_capped(stderr_pipe, stderr),
                wait,
            )?;
            Ok(status)
        };
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(ProbeFault::Cancelled),
            _ = tokio::time::sleep_until(self.deadline) => Err(ProbeFault::Expired),
            result = run => result,
        }
    }
}

impl Inner {
    async fn reaped(&self, child: &mut Child) -> std::io::Result<ExitStatus> {
        #[cfg(test)]
        self.seams.reap_fault()?;
        child.wait().await
    }

    /// Kills only the observation client it spawned and keeps the slot until wait confirms it.
    async fn reap(&self, child: &mut Child, session: &str) {
        self.stats.reaping.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let mut warned = false;
        loop {
            let _ = child.start_kill();
            match tokio::time::timeout(REAP_WARN_AFTER, self.reaped(child)).await {
                Ok(Ok(_)) => break,
                Ok(Err(_)) => tokio::time::sleep(REAP_RETRY).await,
                Err(_) => {}
            }
            if !warned && started.elapsed() >= REAP_WARN_AFTER {
                warned = true;
                let reaping = self.stats.reaping.load(Ordering::Relaxed);
                tracing::warn!(
                    session,
                    reaping,
                    "health tmux probe child is not reaped yet; its slot stays held"
                );
            }
        }
        self.stats.reaping.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn read_capped(
    pipe: Option<impl AsyncRead + Unpin>,
    buffer: &mut Vec<u8>,
) -> Result<(), ProbeFault> {
    let Some(mut pipe) = pipe else {
        return Ok(());
    };
    let mut chunk = [0; 1024];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => return Ok(()),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(_) => return Err(ProbeFault::Io),
        }
        if buffer.len() > OUTPUT_LIMIT {
            return Err(ProbeFault::Overflow);
        }
    }
}

impl TmuxObservationBudget {
    /// Charges only this probe's wait; a spent budget admits nothing and starts no worker.
    pub(super) async fn observe(&mut self, query: HealthTmuxQuery, session: &str) -> ProbeOutcome {
        if self.remaining.is_zero() {
            return ProbeOutcome::Unwitnessed(ProbeFault::Expired);
        }
        let started = Instant::now();
        let outcome = self.runtime.observe(query, session, self.remaining).await;
        self.remaining = self.remaining.saturating_sub(started.elapsed());
        if let ProbeOutcome::Unwitnessed(fault) = outcome {
            let stats = &self.runtime.inner.stats;
            let (children, reaping) = (
                stats.children.load(Ordering::Relaxed),
                stats.reaping.load(Ordering::Relaxed),
            );
            tracing::debug!(
                ?query,
                ?fault,
                children,
                reaping,
                "health tmux probe unwitnessed"
            );
        }
        outcome
    }

    /// The detail PID field, filled after every presence probe from at most 250ms of what is left.
    pub(in crate::services::discord::health) async fn with_process_presence(
        &mut self,
        mut mailboxes: Vec<MailboxHealthSnapshot>,
    ) -> Vec<MailboxHealthSnapshot> {
        self.remaining = self.remaining.min(DETAIL_PID_BUDGET);
        for mailbox in &mut mailboxes {
            let Some(session) = mailbox.relay_health.tmux_session.as_deref() else {
                continue;
            };
            if host_liveness::local_tmux(session, None) {
                let outcome = self.observe(HealthTmuxQuery::PanePid, session).await;
                mailbox.process_present = matches!(
                    outcome,
                    ProbeOutcome::Answered(HealthTmuxAnswer::PanePid(_))
                );
            }
        }
        mailboxes
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::services::platform::tmux::SessionPresence;

    /// Test-only replacements at the spawn boundary; production runtimes carry the default.
    #[derive(Default)]
    pub(super) struct Seams {
        scripted: Option<SessionPresence>,
        calls: [AtomicUsize; 2],
        #[cfg(unix)]
        fake: Option<unix::FakeTmux>,
        reap_faults: AtomicBool,
    }

    impl Seams {
        pub(super) fn scripted(&self, query: HealthTmuxQuery) -> Option<ProbeOutcome> {
            let presence = self.scripted?;
            self.calls[query as usize].fetch_add(1, Ordering::SeqCst);
            Some(ProbeOutcome::Answered(match (presence, query) {
                (SessionPresence::Present, HealthTmuxQuery::HasSession) => {
                    HealthTmuxAnswer::Present
                }
                (SessionPresence::Present, HealthTmuxQuery::PanePid) => {
                    HealthTmuxAnswer::PanePid(4242)
                }
                (SessionPresence::Missing, _) => HealthTmuxAnswer::Missing,
                (SessionPresence::ProbeFailed, _) => HealthTmuxAnswer::Failed,
            }))
        }

        #[cfg(unix)]
        pub(super) fn prepare(&self) -> Option<impl FnOnce() -> Option<Command> + '_> {
            let fake = self.fake.as_ref()?;
            Some(move || fake.prepare())
        }

        #[cfg(not(unix))]
        pub(super) fn prepare(&self) -> Option<fn() -> Option<Command>> {
            None
        }

        pub(super) fn reap_fault(&self) -> std::io::Result<()> {
            if self.reap_faults.load(Ordering::SeqCst) {
                return Err(std::io::Error::other("injected reap fault"));
            }
            Ok(())
        }
    }

    impl HealthProbeRuntime {
        fn with_seams(limit: usize, seams: Seams) -> Self {
            let stats = ProbeStats::default();
            let slots = Semaphore::new(limit);
            Self {
                inner: Arc::new(Inner {
                    slots,
                    stats,
                    seams,
                }),
            }
        }

        /// Answers every query from `presence` without a process, as the fn-pointer seam did.
        pub(in crate::services::discord::health) fn answering(presence: SessionPresence) -> Self {
            let seams = Seams {
                scripted: Some(presence),
                ..Seams::default()
            };
            Self::with_seams(HEALTH_TMUX_MAX_INFLIGHT, seams)
        }

        pub(in crate::services::discord::health) fn is(&self, other: &Self) -> bool {
            Arc::ptr_eq(&self.inner, &other.inner)
        }

        pub(in crate::services::discord::health) fn calls(&self, query: HealthTmuxQuery) -> usize {
            self.inner.seams.calls[query as usize].load(Ordering::SeqCst)
        }
    }

    impl TmuxObservationBudget {
        fn with(runtime: HealthProbeRuntime, remaining: Duration) -> Self {
            Self { remaining, runtime }
        }

        /// A full budget whose probe answers every session with `presence`.
        pub(in crate::services::discord::health) fn answering(presence: SessionPresence) -> Self {
            Self::with(
                HealthProbeRuntime::answering(presence),
                super::super::TMUX_OBSERVATION_BUDGET,
            )
        }

        /// A spent budget whose probe would answer `Present` if the budget check were skipped.
        pub(in crate::services::discord::health) fn exhausted() -> Self {
            Self::with(
                HealthProbeRuntime::answering(SessionPresence::Present),
                Duration::ZERO,
            )
        }
    }

    #[cfg(unix)]
    mod unix {
        use std::path::Path;
        use std::process::Command;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        use poise::serenity_prelude::ChannelId;
        use serde_json::Value;

        use super::super::{
            HealthProbeRuntime, HealthTmuxAnswer, HealthTmuxQuery, ProbeFault, ProbeOutcome,
            TmuxObservationBudget,
        };
        use super::Seams;
        use crate::services::discord::health::session_enrichment::{
            ExecutorWitness, HealthSnapshotOptions, witness_tmux_session_within,
        };
        use crate::services::discord::health::{self, HealthRegistry};
        use crate::services::platform::tmux::health_probe::{
            self as tmux_probe, HealthTmuxRefusal,
        };

        /// Answers by target prefix and logs `pid|argv` for every process it becomes.
        const FAKE_TMUX: &str = r#"#!/bin/sh
printf '%s|%s\n' "$$" "$*" >> '@LOG@'
query="$3"
case "$query" in has-session|display-message) ;; *) echo "unexpected tmux command: $*" >&2; exit 3 ;; esac
if [ "$query" = display-message ]; then target="$6"; else target="$5"; fi
case "$target" in
=alive-*) [ "$query" = has-session ] && exit 0; echo 4242; exit 0 ;;
=gone-*) echo "can't find session: $target" >&2; exit 1 ;;
=fault-*) echo "protocol version mismatch" >&2; exit 1 ;;
=hold-*) exec sleep 30 ;;
=slowpid-*) [ "$query" = has-session ] && exit 0; exec sleep 30 ;;
=late-*) echo "can't find session: $target" >&2; exec sleep 30 ;;
=over-*) i=0; while [ $i -lt 80 ]; do printf '%063d\n' 0; i=$((i+1)); done; exec sleep 30 ;;
=floodout-*) printf 'x\n' >&2; i=0; while [ $i -lt 1600 ]; do printf '%063d\n' 0; i=$((i+1)); done ;;
=flooderr-*) printf 'x\n'; i=0; while [ $i -lt 1600 ]; do printf '%063d\n' 0 >&2; i=$((i+1)); done ;;
=sentinel-*) [ "$query" = has-session ] && exit 0; read pid < '@SENTINEL@'; echo "$pid"; exit 0 ;;
*) echo "unexpected fake tmux call: $*" >&2; exit 3 ;;
esac
"#;

        const FRESH_PROCESS_TEST: &str = "services::discord::health::session_enrichment::probe_runtime::tests::unix::a_fresh_process_shares_one_runtime_across_both_snapshot_builds";
        const FRESH_PROCESS_ENV: &str = "ADK_HEALTH_PROBE_FRESH_PROCESS";

        pub(in super::super) struct FakeTmux {
            dir: tempfile::TempDir,
            prepare_delay: Duration,
            cold: bool,
        }

        impl FakeTmux {
            fn new() -> Self {
                let dir = tempfile::tempdir().expect("fake tmux dir");
                write_fake_tmux(dir.path());
                Self {
                    dir,
                    prepare_delay: Duration::ZERO,
                    cold: false,
                }
            }

            /// The `tmux -u` the prepared PATH would yield, or nothing for a cold PATH.
            pub(in super::super) fn prepare(&self) -> Option<Command> {
                std::thread::sleep(self.prepare_delay);
                let mut command = Command::new("/bin/sh");
                command.arg(self.dir.path().join("tmux")).arg("-u");
                (!self.cold).then_some(command)
            }
        }

        fn write_fake_tmux(dir: &Path) {
            use std::os::unix::fs::PermissionsExt;
            let script = FAKE_TMUX
                .replace("@LOG@", &dir.join("spawns").to_string_lossy())
                .replace("@SENTINEL@", &dir.join("sentinel.pid").to_string_lossy());
            let path = dir.join("tmux");
            std::fs::write(&path, script).expect("write fake tmux");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        /// `(pid, argv)` of every process the fake became, in start order.
        fn spawns(dir: &Path) -> Vec<(i32, String)> {
            let log = std::fs::read_to_string(dir.join("spawns")).unwrap_or_default();
            log.lines()
                .filter_map(|line| {
                    let (pid, args) = line.split_once('|')?;
                    Some((pid.parse().ok()?, args.to_string()))
                })
                .collect()
        }

        fn alive(pid: i32) -> bool {
            unsafe { libc::kill(pid, 0) == 0 }
        }

        impl HealthProbeRuntime {
            fn faked(limit: usize, fake: FakeTmux) -> Self {
                Self::with_seams(
                    limit,
                    Seams {
                        fake: Some(fake),
                        ..Seams::default()
                    },
                )
            }

            fn fake_dir(&self) -> &Path {
                self.inner
                    .seams
                    .fake
                    .as_ref()
                    .expect("faked runtime")
                    .dir
                    .path()
            }

            /// `(workers, children, reaping)` right now.
            fn gauges(&self) -> (usize, usize, usize) {
                let stats = &self.inner.stats;
                let load = |gauge: &std::sync::atomic::AtomicUsize| gauge.load(Ordering::SeqCst);
                (
                    load(&stats.workers),
                    load(&stats.children),
                    load(&stats.reaping),
                )
            }

            async fn settles_within(&self, limit: usize, within: Duration) -> bool {
                let until = std::time::Instant::now() + within;
                while std::time::Instant::now() < until {
                    if self.gauges() == (0, 0, 0) && self.inner.slots.available_permits() == limit {
                        return true;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                false
            }
        }

        async fn spawned_at_least(dir: &Path, count: usize) -> bool {
            for _ in 0..1000 {
                if spawns(dir).len() >= count {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            false
        }

        fn faked() -> HealthProbeRuntime {
            HealthProbeRuntime::faked(4, FakeTmux::new())
        }

        fn budget(runtime: &HealthProbeRuntime, remaining: Duration) -> TmuxObservationBudget {
            TmuxObservationBudget::with(runtime.clone(), remaining)
        }

        fn watcher_handle(session: &str) -> crate::services::discord::TmuxWatcherHandle {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};
            crate::services::discord::TmuxWatcherHandle {
                tmux_session_name: session.to_string(),
                output_path: "/tmp/agentdesk-probe-runtime.jsonl".to_string(),
                paused: Arc::new(AtomicBool::new(false)),
                resume_offset: Arc::new(std::sync::Mutex::new(None)),
                cancel: Arc::new(AtomicBool::new(false)),
                pause_epoch: Arc::new(AtomicU64::new(0)),
                turn_delivered: Arc::new(AtomicBool::new(false)),
                last_heartbeat_ts_ms: Arc::new(AtomicI64::new(
                    crate::services::discord::tmux_watcher_now_ms(),
                )),
            }
        }

        /// One codex provider whose channels are bound to `sessions`, in channel order.
        async fn registry_with(sessions: &[&str], first_channel: u64) -> HealthRegistry {
            let registry = HealthRegistry::new();
            let shared = crate::services::discord::make_shared_data_for_tests();
            for (offset, session) in (0u64..).zip(sessions) {
                let channel = ChannelId::new(first_channel + offset);
                shared.mailboxes.handle(channel);
                shared
                    .tmux_watchers
                    .insert(channel, watcher_handle(session));
            }
            registry.register("codex".to_string(), shared).await;
            registry
        }

        fn mailboxes(json: &Value) -> Vec<Value> {
            json["mailboxes"].as_array().cloned().unwrap_or_default()
        }

        fn relay_reasons(json: &Value) -> Vec<String> {
            let reasons = json["degraded_reasons"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            reasons
                .iter()
                .filter_map(Value::as_str)
                .filter(|reason| reason.starts_with("relay_verdict_"))
                .map(str::to_string)
                .collect()
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn admission_holds_each_slot_until_its_child_is_reaped() {
            let runtime = faked();
            runtime
                .inner
                .seams
                .reap_faults
                .store(true, Ordering::SeqCst);
            let requests: Vec<_> = (0..4)
                .map(|n| {
                    let runtime = runtime.clone();
                    tokio::spawn(async move {
                        let session = format!("hold-{n}");
                        runtime
                            .observe(
                                HealthTmuxQuery::HasSession,
                                &session,
                                Duration::from_millis(300),
                            )
                            .await
                    })
                })
                .collect();
            for request in requests {
                let outcome = request.await.expect("request task");
                assert_eq!(outcome, ProbeOutcome::Unwitnessed(ProbeFault::Expired));
            }
            assert_eq!(
                spawns(runtime.fake_dir()).len(),
                4,
                "fixture: four children must start"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(
                runtime.gauges(),
                (4, 4, 4),
                "every expired child is still unreaped"
            );
            let fifth = runtime
                .observe(
                    HealthTmuxQuery::HasSession,
                    "hold-5",
                    Duration::from_secs(1),
                )
                .await;
            assert_eq!(fifth, ProbeOutcome::Unwitnessed(ProbeFault::Saturated));
            assert_eq!(
                spawns(runtime.fake_dir()).len(),
                4,
                "no slot may be reused before its reap"
            );
            runtime
                .inner
                .seams
                .reap_faults
                .store(false, Ordering::SeqCst);
            let opened = std::time::Instant::now();
            assert!(
                runtime.settles_within(4, Duration::from_secs(1)).await,
                "{:?}",
                runtime.gauges()
            );
            println!("reap_fault_cleared_to_settled={:?}", opened.elapsed());
            for (pid, _) in spawns(runtime.fake_dir()) {
                assert!(!alive(pid), "child {pid} was released without being reaped");
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn the_child_deadline_is_the_remaining_budget_not_three_seconds() {
            let runtime = faked();
            let mut short = budget(&runtime, Duration::from_millis(100));
            let started = std::time::Instant::now();
            let witness = witness_tmux_session_within(Some("hold-deadline"), &mut short).await;
            assert_eq!(witness, ExecutorWitness::Unwitnessed);
            assert!(
                started.elapsed() < Duration::from_millis(600),
                "{:?}",
                started.elapsed()
            );
            assert!(
                short.remaining.is_zero(),
                "the wait is charged to the budget"
            );
            assert!(
                runtime.settles_within(4, Duration::from_secs(1)).await,
                "the child outlived its budget"
            );
            println!("budget_start_to_settled={:?}", started.elapsed());
            let spawned = spawns(runtime.fake_dir());
            assert_eq!(spawned.len(), 1);
            assert!(!alive(spawned[0].0));

            // The owner keeps the deadline even while the request is neither polled nor dropped.
            let runtime = faked();
            let idle = runtime.observe(
                HealthTmuxQuery::HasSession,
                "hold-idle",
                Duration::from_millis(100),
            );
            let mut idle = Box::pin(idle);
            assert!(futures::poll!(idle.as_mut()).is_pending());
            assert!(
                spawned_at_least(runtime.fake_dir(), 1).await,
                "fixture: the child must start"
            );
            assert!(
                runtime.settles_within(4, Duration::from_secs(1)).await,
                "the child waited on its request instead of its own deadline"
            );
            drop(idle);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn dropping_the_request_cancels_and_reaps_its_running_child() {
            let runtime = faked();
            let request = tokio::spawn({
                let runtime = runtime.clone();
                async move {
                    let ten_seconds = Duration::from_secs(10);
                    runtime
                        .observe(HealthTmuxQuery::HasSession, "hold-cancel", ten_seconds)
                        .await
                }
            });
            assert!(
                spawned_at_least(runtime.fake_dir(), 1).await,
                "fixture: the child must start"
            );
            let aborted = std::time::Instant::now();
            request.abort();
            assert!(request.await.expect_err("aborted").is_cancelled());
            assert!(
                runtime.settles_within(4, Duration::from_secs(1)).await,
                "{:?}",
                runtime.gauges()
            );
            println!("cancel_to_settled={:?}", aborted.elapsed());
            assert!(
                !alive(spawns(runtime.fake_dir())[0].0),
                "the cancelled child must be reaped"
            );
        }

        #[tokio::test(flavor = "current_thread")]
        async fn a_request_gone_before_spawn_creates_no_child() {
            let runtime = faked();
            let mut request = Box::pin(runtime.observe(
                HealthTmuxQuery::HasSession,
                "hold-early",
                Duration::from_secs(10),
            ));
            assert!(futures::poll!(request.as_mut()).is_pending());
            assert_eq!(runtime.gauges().0, 1, "fixture: the owner was admitted");
            drop(request);
            assert!(
                runtime.settles_within(4, Duration::from_secs(1)).await,
                "{:?}",
                runtime.gauges()
            );
            let spawned = runtime.inner.stats.spawned.load(Ordering::SeqCst);
            let logged = spawns(runtime.fake_dir()).len();
            assert_eq!(
                (spawned, logged),
                (0, 0),
                "a dropped request spawned a child"
            );

            // The deadline passes while the command is prepared; the pre-spawn check must stop it.
            let slow = FakeTmux {
                prepare_delay: Duration::from_millis(150),
                ..FakeTmux::new()
            };
            let runtime = HealthProbeRuntime::faked(4, slow);
            let outcome = runtime
                .observe(
                    HealthTmuxQuery::HasSession,
                    "hold-late",
                    Duration::from_millis(50),
                )
                .await;
            assert_eq!(outcome, ProbeOutcome::Unwitnessed(ProbeFault::Expired));
            assert!(
                runtime.settles_within(4, Duration::from_secs(1)).await,
                "{:?}",
                runtime.gauges()
            );
            let spawned = runtime.inner.stats.spawned.load(Ordering::SeqCst);
            let logged = spawns(runtime.fake_dir()).len();
            assert_eq!(
                (spawned, logged),
                (0, 0),
                "an expired admission spawned a child"
            );
        }

        #[test]
        fn a_spent_budget_starts_no_worker_for_presence_or_pid() {
            let _root = crate::config::TestRuntimeRootGuard::new();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async {
                let runtime = faked();
                let mut spent = budget(&runtime, Duration::ZERO);
                let witness = witness_tmux_session_within(Some("alive-spent"), &mut spent).await;
                assert_eq!(witness, ExecutorWitness::Unwitnessed);
                let registry = registry_with(&["alive-spent"], 5_736_100_000_000_001).await;
                let tmux = budget(&runtime, Duration::ZERO);
                let options = HealthSnapshotOptions {
                    include_mailbox_details: true,
                    tmux,
                };
                let snapshot =
                    health::snapshot::build_health_snapshot_with_options(&registry, options).await;
                let json = serde_json::to_value(snapshot).expect("json");
                assert_eq!(
                    mailboxes(&json).len(),
                    1,
                    "fixture: the channel must reach the PID pass"
                );
                assert_eq!(mailboxes(&json)[0]["process_present"], false);
                assert_eq!(
                    runtime.inner.stats.admitted.load(Ordering::SeqCst),
                    0,
                    "a worker was admitted"
                );
                assert!(spawns(runtime.fake_dir()).is_empty());
            });
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn every_non_answer_stays_unwitnessed_and_only_missing_is_absent() {
            let runtime = faked();
            let cases = [
                ("alive-w", ExecutorWitness::Present),
                ("gone-w", ExecutorWitness::Absent),
                ("fault-w", ExecutorWitness::Unwitnessed),
                ("hold-w", ExecutorWitness::Unwitnessed),
                ("over-w", ExecutorWitness::Unwitnessed),
                ("other-w", ExecutorWitness::Unwitnessed),
                ("   ", ExecutorWitness::Unwitnessed),
            ];
            for (session, expected) in cases {
                let mut probe = budget(&runtime, Duration::from_millis(300));
                let witness = witness_tmux_session_within(Some(session), &mut probe).await;
                assert_eq!(witness, expected, "{session:?}");
            }
            let mut spent = budget(&runtime, Duration::ZERO);
            assert_eq!(
                witness_tmux_session_within(None, &mut spent).await,
                ExecutorWitness::Absent
            );
            let started: Vec<_> = spawns(runtime.fake_dir())
                .into_iter()
                .map(|(_, argv)| argv)
                .collect();
            assert_eq!(
                started.len(),
                6,
                "the blank name must not reach tmux: {started:?}"
            );

            let full = HealthProbeRuntime::faked(0, FakeTmux::new());
            let mut saturated = budget(&full, Duration::from_secs(1));
            let witness = witness_tmux_session_within(Some("alive-full"), &mut saturated).await;
            assert_eq!(witness, ExecutorWitness::Unwitnessed);
            let cold = HealthProbeRuntime::faked(
                4,
                FakeTmux {
                    cold: true,
                    ..FakeTmux::new()
                },
            );
            let mut unready = budget(&cold, Duration::from_secs(1));
            let witness = witness_tmux_session_within(Some("alive-cold"), &mut unready).await;
            assert_eq!(witness, ExecutorWitness::Unwitnessed);
            assert!(spawns(full.fake_dir()).is_empty() && spawns(cold.fake_dir()).is_empty());
            assert!(
                runtime.settles_within(4, Duration::from_secs(1)).await,
                "{:?}",
                runtime.gauges()
            );
        }

        #[test]
        fn the_detail_pid_probe_runs_after_presence_within_250ms_and_public_spawns_none() {
            use crate::config::RelayVerdictSource;
            use crate::services::discord::health::reachability::composite::set_relay_verdict_source_for_tests;
            let _root = crate::config::TestRuntimeRootGuard::new();
            let _source = set_relay_verdict_source_for_tests(RelayVerdictSource::Composite);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async {
                let build = |runtime: &HealthProbeRuntime, include_mailbox_details: bool| {
                    let tmux = budget(runtime, Duration::from_secs(2));
                    HealthSnapshotOptions {
                        include_mailbox_details,
                        tmux,
                    }
                };
                let runtime = faked();
                let registry = registry_with(&["alive-pid"], 5_736_100_000_000_011).await;
                let snapshot = health::snapshot::build_health_snapshot_with_options(
                    &registry,
                    build(&runtime, true),
                );
                let json = serde_json::to_value(snapshot.await).expect("json");
                assert_eq!(mailboxes(&json)[0]["tmux_present"], true);
                assert_eq!(
                    mailboxes(&json)[0]["process_present"],
                    true,
                    "the PID came through the runtime"
                );
                let argv: Vec<_> = spawns(runtime.fake_dir())
                    .into_iter()
                    .map(|(_, argv)| argv)
                    .collect();
                assert_eq!(
                    argv,
                    [
                        "-u -N has-session -t =alive-pid:",
                        "-u -N display-message -p -t =alive-pid: #{pane_pid}"
                    ]
                );

                let runtime = faked();
                let registry =
                    registry_with(&["slowpid-a", "slowpid-b"], 5_736_100_000_000_021).await;
                let gap = std::sync::Arc::new(std::sync::Mutex::new(Duration::ZERO));
                let ticker = tokio::spawn({
                    let gap = gap.clone();
                    async move {
                        let mut last = std::time::Instant::now();
                        loop {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                            let mut widest =
                                gap.lock().unwrap_or_else(|poison| poison.into_inner());
                            *widest = (*widest).max(last.elapsed());
                            last = std::time::Instant::now();
                        }
                    }
                });
                let started = std::time::Instant::now();
                let snapshot = health::snapshot::build_health_snapshot_with_options(
                    &registry,
                    build(&runtime, true),
                );
                let json = serde_json::to_value(snapshot.await).expect("json");
                let elapsed = started.elapsed();
                ticker.abort();
                for mailbox in mailboxes(&json) {
                    assert_eq!(
                        mailbox["tmux_present"], true,
                        "a PID probe took a presence budget"
                    );
                    assert_eq!(mailbox["process_present"], false);
                }
                assert!(
                    elapsed < Duration::from_millis(900),
                    "PID probes ran past 250ms: {elapsed:?}"
                );
                let pid_probes = spawns(runtime.fake_dir())
                    .iter()
                    .filter(|(_, argv)| argv.contains("display-message"))
                    .count();
                assert_eq!(pid_probes, 1, "the second PID probe had no budget left");
                let widest = *gap.lock().unwrap_or_else(|poison| poison.into_inner());
                assert!(
                    widest < Duration::from_millis(150),
                    "the runtime thread was blocked for {widest:?}"
                );
                assert!(
                    runtime.settles_within(4, Duration::from_secs(1)).await,
                    "{:?}",
                    runtime.gauges()
                );

                let runtime = faked();
                let registry = registry_with(&["alive-public"], 5_736_100_000_000_031).await;
                let snapshot = health::snapshot::build_health_snapshot_with_options(
                    &registry,
                    build(&runtime, false),
                );
                let json = serde_json::to_value(snapshot.await).expect("json");
                assert!(mailboxes(&json).is_empty());
                let argv: Vec<_> = spawns(runtime.fake_dir())
                    .into_iter()
                    .map(|(_, argv)| argv)
                    .collect();
                assert_eq!(
                    argv,
                    ["-u -N has-session -t =alive-public:"],
                    "public must observe presence only"
                );
            });
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn both_pipes_flooded_end_in_overflow_without_a_deadlock() {
            let runtime = faked();
            for session in ["flooderr-a", "floodout-a"] {
                let started = std::time::Instant::now();
                let outcome = runtime
                    .observe(HealthTmuxQuery::HasSession, session, Duration::from_secs(3))
                    .await;
                assert_eq!(
                    outcome,
                    ProbeOutcome::Unwitnessed(ProbeFault::Overflow),
                    "{session}"
                );
                assert!(
                    started.elapsed() < Duration::from_secs(1),
                    "{session} stalled on a full pipe"
                );
            }
            assert!(
                runtime.settles_within(4, Duration::from_secs(1)).await,
                "{:?}",
                runtime.gauges()
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn an_output_or_wait_fault_keeps_the_slot_until_reap_confirms() {
            let runtime = faked();
            runtime
                .inner
                .seams
                .reap_faults
                .store(true, Ordering::SeqCst);
            let outcome = runtime
                .observe(
                    HealthTmuxQuery::HasSession,
                    "over-b",
                    Duration::from_secs(2),
                )
                .await;
            assert_eq!(outcome, ProbeOutcome::Unwitnessed(ProbeFault::Overflow));
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                runtime.gauges(),
                (1, 1, 1),
                "the slot is held while wait keeps failing"
            );
            assert_eq!(runtime.inner.slots.available_permits(), 3);
            runtime
                .inner
                .seams
                .reap_faults
                .store(false, Ordering::SeqCst);
            assert!(
                runtime.settles_within(4, Duration::from_secs(1)).await,
                "{:?}",
                runtime.gauges()
            );
            assert!(!alive(spawns(runtime.fake_dir())[0].0));
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_missing_answer_after_the_deadline_is_not_adopted() {
            let runtime = faked();
            let mut short = budget(&runtime, Duration::from_millis(300));
            let witness = witness_tmux_session_within(Some("late-a"), &mut short).await;
            assert_eq!(
                witness,
                ExecutorWitness::Unwitnessed,
                "a late missing answer expired a ledger"
            );
            assert!(
                runtime.settles_within(4, Duration::from_secs(1)).await,
                "{:?}",
                runtime.gauges()
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn only_the_two_queries_run_and_only_the_spawned_client_is_killed() {
            let cold = tmux_probe::health_tmux_command_using(
                HealthTmuxQuery::HasSession,
                "alive-x",
                || None,
            );
            assert_eq!(cold.err(), Some(HealthTmuxRefusal::RuntimePathUnready));
            let runtime = faked();
            let mut sentinel = Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("sentinel pane");
            let sentinel_pid = sentinel.id();
            std::fs::write(
                runtime.fake_dir().join("sentinel.pid"),
                sentinel_pid.to_string(),
            )
            .expect("pid file");
            let mut probe = budget(&runtime, Duration::from_secs(2));
            let answer = probe.observe(HealthTmuxQuery::PanePid, "sentinel-k").await;
            assert_eq!(
                answer,
                ProbeOutcome::Answered(HealthTmuxAnswer::PanePid(sentinel_pid))
            );
            let mut short = budget(&runtime, Duration::from_millis(200));
            assert!(matches!(
                short.observe(HealthTmuxQuery::PanePid, "slowpid-k").await,
                ProbeOutcome::Unwitnessed(_)
            ));
            assert!(
                runtime.settles_within(4, Duration::from_secs(1)).await,
                "{:?}",
                runtime.gauges()
            );
            let sentinel_alive = sentinel.try_wait().expect("sentinel status").is_none();
            let _ = sentinel.kill();
            let _ = sentinel.wait();
            assert!(
                sentinel_alive,
                "the pane PID the probe read must never be signalled"
            );
            let argv: Vec<_> = spawns(runtime.fake_dir())
                .into_iter()
                .map(|(_, argv)| argv)
                .collect();
            assert_eq!(
                argv,
                [
                    "-u -N display-message -p -t =sentinel-k: #{pane_pid}",
                    "-u -N display-message -p -t =slowpid-k: #{pane_pid}",
                ]
            );
        }

        /// Runs in a fresh child process so the PATH starts cold and the shared runtime has no other users.
        #[test]
        fn a_fresh_process_shares_one_runtime_across_both_snapshot_builds() {
            if let Some(bin) = std::env::var_os(FRESH_PROCESS_ENV) {
                return fresh_process_scenario(Path::new(&bin));
            }
            let dir = tempfile::tempdir().expect("fresh process dir");
            let [bin, home, root] = ["bin", "home", "root"].map(|name| dir.path().join(name));
            for path in [&bin, &home, &root] {
                std::fs::create_dir_all(path).expect("fixture dir");
            }
            write_fake_tmux(&bin);
            let inherited = std::env::var_os("PATH").unwrap_or_default();
            let path = std::env::join_paths(
                std::iter::once(bin.clone()).chain(std::env::split_paths(&inherited)),
            );
            let mut command = Command::new(std::env::current_exe().expect("test binary"));
            command
                .args(["--exact", FRESH_PROCESS_TEST, "--nocapture"])
                .env(FRESH_PROCESS_ENV, &bin)
                .env("PATH", path.expect("PATH"))
                .env("SHELL", "/bin/sh")
                .env("HOME", &home)
                .env("AGENTDESK_ROOT_DIR", &root)
                .env_remove("TMUX")
                .env_remove("TMUX_PANE")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            crate::services::process::configure_child_process_group(&mut command);
            let child = command.spawn().expect("spawn fresh process");
            let label = "health probe fresh process";
            let output = crate::services::process::wait_with_output_timeout(
                child,
                Duration::from_secs(120),
                label,
            );
            let output = output.expect("fresh process finished");
            let (stdout, stderr) = (
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            assert!(output.status.success(), "{stdout}\n{stderr}");
            assert!(
                stdout.contains("test result: ok. 1 passed"),
                "the scenario was filtered out: {stdout}"
            );
            assert!(stdout.contains("HEALTH_PROBE_FRESH_PROCESS_OK"), "{stdout}");
            print!("{stdout}");
        }

        fn fresh_process_scenario(bin: &Path) {
            use crate::config::RelayVerdictSource;
            use crate::services::discord::health::reachability::composite::set_relay_verdict_source_for_tests;
            use crate::services::platform::binary_resolver;
            let _source = set_relay_verdict_source_for_tests(RelayVerdictSource::Composite);
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build();
            rt.expect("runtime").block_on(async {
                let shared = HealthProbeRuntime::shared();
                assert!(
                    binary_resolver::prepared_runtime_path().is_none(),
                    "fixture: the PATH must start cold"
                );
                let cold = registry_with(&["alive-cold"], 5_736_200_000_000_001).await;
                let json =
                    serde_json::to_value(health::build_health_snapshot(&cold).await).expect("json");
                assert_eq!(
                    mailboxes(&json).len(),
                    1,
                    "fixture: the cold channel must be observed"
                );
                assert_eq!(mailboxes(&json)[0]["tmux_present"], false);
                let mut cold_budget = HealthSnapshotOptions::new(false).tmux;
                let outcome = cold_budget
                    .observe(HealthTmuxQuery::HasSession, "alive-cold")
                    .await;
                assert_eq!(outcome, ProbeOutcome::Unwitnessed(ProbeFault::PathUnready));
                assert!(
                    spawns(bin).is_empty(),
                    "a cold PATH spawned tmux or a shell"
                );
                assert!(
                    binary_resolver::prepared_runtime_path().is_none(),
                    "health prepared the PATH itself"
                );

                assert!(
                    binary_resolver::merged_runtime_path().is_some(),
                    "fixture: PATH preparation"
                );
                assert!(binary_resolver::prepared_runtime_path().is_some());
                let mut warm = HealthSnapshotOptions::new(false).tmux;
                assert_eq!(
                    witness_tmux_session_within(Some("alive-1"), &mut warm).await,
                    ExecutorWitness::Present
                );
                let mut warm = HealthSnapshotOptions::new(true).tmux;
                assert_eq!(
                    witness_tmux_session_within(Some("gone-1"), &mut warm).await,
                    ExecutorWitness::Absent
                );
                let argv: Vec<_> = spawns(bin).into_iter().map(|(_, argv)| argv).collect();
                assert_eq!(
                    argv,
                    [
                        "-u -N has-session -t =alive-1:",
                        "-u -N has-session -t =gone-1:"
                    ]
                );

                let parity = registry_with(&["gone-parity"], 5_736_200_000_000_011).await;
                let detail = serde_json::to_value(health::build_health_snapshot(&parity).await)
                    .expect("json");
                let public =
                    serde_json::to_value(health::build_public_health_snapshot(&parity).await)
                        .expect("json");
                assert!(
                    !relay_reasons(&detail).is_empty(),
                    "fixture: the parity channel must be non-green"
                );
                assert_eq!(detail["status"], public["status"]);
                assert_eq!(relay_reasons(&detail), relay_reasons(&public));
                assert_eq!((mailboxes(&detail).len(), mailboxes(&public).len()), (1, 0));

                let before = spawns(bin).len();
                let saturated_before = shared.inner.stats.saturated.load(Ordering::SeqCst);
                let names = [
                    "hold-detail-1",
                    "hold-public-1",
                    "hold-detail-2",
                    "hold-public-2",
                    "hold-detail-3",
                ];
                let mut builds = Vec::new();
                for (channel, name) in (5_736_200_000_000_021u64..).zip(names) {
                    let registry = registry_with(&[name], channel).await;
                    builds.push(tokio::spawn(async move {
                        if name.contains("public") {
                            drop(health::build_public_health_snapshot(&registry).await);
                        } else {
                            drop(health::build_health_snapshot(&registry).await);
                        }
                    }));
                }
                let mut widest = 0;
                while builds.iter().any(|build| !build.is_finished()) {
                    widest = widest.max(shared.inner.stats.children.load(Ordering::SeqCst));
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                for build in builds {
                    build.await.expect("build task");
                }
                let held: Vec<_> = spawns(bin).split_off(before);
                assert_eq!(
                    held.len(),
                    4,
                    "the shared runtime must cap children at four: {held:?}"
                );
                assert!(
                    held.iter().any(|(_, argv)| argv.contains("hold-public")),
                    "{held:?}"
                );
                assert!(
                    held.iter().any(|(_, argv)| argv.contains("hold-detail")),
                    "{held:?}"
                );
                assert_eq!(widest, 4, "four children were held at once");
                assert!(shared.inner.stats.saturated.load(Ordering::SeqCst) > saturated_before);
                assert!(
                    shared.settles_within(4, Duration::from_secs(1)).await,
                    "{:?}",
                    shared.gauges()
                );
                assert!(held.iter().all(|(pid, _)| !alive(*pid)), "{held:?}");
                println!(
                    "HEALTH_PROBE_FRESH_PROCESS_OK held={} widest={widest}",
                    held.len()
                );
            });
        }
    }
}
