//! Dormant native-clear orchestration. Canonical binding records alone decide commitment.

use super::{
    binding_context::{BindingContext, SpawnNonceMarker, observe_spawn_nonce_marker},
    binding_events::{self, BindingCause, BindingEvent, BindingTarget, SourceId},
};
use crate::services::{
    claude_tui::host_input::NativeClearSubmission,
    tmux_common::{self, TmuxSourceAuthority},
};
use std::{future::Future, io, pin::Pin, time::Duration};
use tokio::{
    sync::{OwnedMutexGuard, oneshot, watch},
    time::Instant,
};

pub(crate) const NATIVE_CLEAR_BUDGET: Duration = Duration::from_secs(20);
const FINISH_RESERVE: Duration = Duration::from_secs(4);
type Step<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClearAdmission {
    Native,
    Fallback,
}

#[derive(Clone, Debug)]
pub(crate) struct ClearTicket {
    pub context: BindingContext,
    pub old: SourceId,
    pub baseline: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClearCommit {
    pub seq: u64,
    pub session: String,
    pub source: Option<SourceId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClearDecision {
    Committed(ClearCommit),
    Fallback,
    Hold(Option<ClearCommit>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClearOutcome {
    Native(ClearCommit),
    Fallback,
    // The caller must keep subsequent admission closed; no reset is authorized.
    Hold(Option<ClearCommit>),
}

#[allow(dead_code)]
pub(crate) struct CanonicalClearWaiter {
    pub ticket: ClearTicket,
    pub changed: watch::Receiver<u64>,
}

#[allow(dead_code)]
impl CanonicalClearWaiter {
    // Subscribe before reading the baseline so a fast hook cannot be lost.
    pub(crate) fn capture(context: BindingContext, old: SourceId) -> io::Result<Self> {
        let channel = context
            .channel_id
            .filter(|id| *id != 0)
            .ok_or_else(|| io::Error::other("clear channel missing"))?;
        if context.provider != "claude" || context.execution_nonce.is_empty() {
            return Err(io::Error::other("unsupported clear execution"));
        }
        let changed = binding_events::subscribe_binding_events(channel)?;
        let mut ticket = ClearTicket {
            context,
            old,
            baseline: 0,
        };
        let baseline = tmux_common::try_with_tmux_source_authority(
            &ticket.context.tmux_session,
            |_| -> io::Result<u64> {
                if !current_execution(&ticket)
                    || binding_events::pinned_source(channel, &ticket.context.tmux_session)?
                        .as_ref()
                        != Some(&ticket.old)
                {
                    return Err(io::Error::other("clear source changed"));
                }
                Ok(binding_events::records_strict(channel)?
                    .map_err(|_| io::Error::other("clear log corrupt"))?
                    .last()
                    .map_or(0, |event| event.seq))
            },
        )
        .ok_or_else(|| io::Error::other("clear source busy"))??;
        ticket.baseline = baseline;
        Ok(Self { ticket, changed })
    }

    pub(crate) fn probe(&self) -> ClearDecision {
        self.decide(|_| ()).0
    }

    pub(crate) fn fallback_cut(
        &self,
        session: &crate::services::session_host::ClearedHostSession,
    ) -> ClearDecision {
        if session.name() != self.ticket.context.tmux_session {
            return ClearDecision::Hold(None);
        }
        self.decide(|authority| {
            tmux_common::cleanup_native_clear_fallback_under_source_authority(authority, session)
        })
        .0
    }

    // Commitment and marker invalidation share the existing source authority.
    fn decide<R>(
        &self,
        fallback: impl FnOnce(&TmuxSourceAuthority<'_>) -> R,
    ) -> (ClearDecision, Option<R>) {
        tmux_common::try_with_tmux_source_authority(
            &self.ticket.context.tmux_session,
            |authority| {
                let records =
                    binding_events::records_strict(self.ticket.context.channel_id.unwrap_or(0));
                let Ok(Ok(records)) = records else {
                    return (ClearDecision::Hold(None), None);
                };
                if let Some(mut commit) = clear_commit(&self.ticket, &records) {
                    if !current_execution(&self.ticket) {
                        return (ClearDecision::Hold(Some(commit)), None);
                    }
                    let history = binding_events::claude_history(
                        self.ticket.context.channel_id.unwrap_or(0),
                        &self.ticket.context.tmux_session,
                        Some(&self.ticket.context.execution_nonce),
                    );
                    let Ok((_, history)) = history else {
                        return (ClearDecision::Hold(Some(commit)), None);
                    };
                    let visit = history.awaiting.as_ref().or(history.current.as_ref());
                    if !history.complete || !visit.is_some_and(|v| v.session == commit.session) {
                        return (ClearDecision::Hold(Some(commit)), None);
                    }
                    commit.source = visit.and_then(|v| v.pin.clone());
                    return (ClearDecision::Committed(commit), None);
                }
                if !current_execution(&self.ticket) {
                    return (ClearDecision::Hold(None), None);
                }
                (ClearDecision::Fallback, Some(fallback(authority)))
            },
        )
        .unwrap_or((ClearDecision::Hold(None), None))
    }
}

fn current_execution(ticket: &ClearTicket) -> bool {
    matches!(observe_spawn_nonce_marker(&ticket.context.tmux_session), SpawnNonceMarker::Known(n) if n == ticket.context.execution_nonce)
}

fn clear_commit(ticket: &ClearTicket, records: &[BindingEvent]) -> Option<ClearCommit> {
    let mut committed = None;
    for event in records.iter().filter(|e| e.seq > ticket.baseline) {
        if event.channel_id != ticket.context.channel_id.unwrap_or(0)
            || event.provider != ticket.context.provider
            || event.tmux_session != ticket.context.tmux_session
            || event.execution_nonce.as_deref() != Some(ticket.context.execution_nonce.as_str())
        {
            continue;
        }
        match &event.new {
            BindingTarget::Source(_) | BindingTarget::Pending { .. }
                if event.cause == BindingCause::Clear
                    && event.evidence.hook_event.as_deref() == Some("session_start")
                    && event.old.as_ref() == Some(&ticket.old) =>
            {
                let (session, source) = match &event.new {
                    BindingTarget::Source(source) => {
                        (source.session_id.clone(), Some(source.clone()))
                    }
                    BindingTarget::Pending {
                        payload_session_id, ..
                    } => (payload_session_id.clone(), None),
                    _ => unreachable!(),
                };
                if !session.trim().is_empty() && session != ticket.old.session_id {
                    committed = Some(ClearCommit {
                        seq: event.seq,
                        session,
                        source,
                    });
                }
            }
            BindingTarget::Resolved {
                pending_seq,
                source,
            } if committed
                .as_ref()
                .is_some_and(|c| c.seq == *pending_seq && c.session == source.session_id) =>
            {
                committed.as_mut()?.source = Some(source.clone());
            }
            _ => {}
        }
    }
    committed
}

// Synchronous callbacks must honor the deadline and hold no lock across an async callback.
// decide(true) must retire the execution under the canonical waiter's fallback fence.
#[allow(dead_code)]
pub(crate) trait NativeClearHost: Send + 'static {
    fn changes(&self) -> watch::Receiver<u64>;
    fn prepare(&mut self, deadline: Instant) -> Step<'_, bool>;
    fn submit(&mut self, deadline: Instant) -> NativeClearSubmission;
    fn decide(&mut self, allow_fallback: bool) -> ClearDecision;
    fn composer_empty(&mut self, deadline: Instant) -> bool;
    fn save(&mut self, commit: ClearCommit, deadline: Instant) -> Step<'_, bool>;
    fn fallback(&mut self, deadline: Instant) -> Step<'_, bool>;
}

// Dropping the receiver never cancels the worker or releases its transition guard early.
#[allow(dead_code)]
pub(crate) fn start_native_clear<H: NativeClearHost>(
    host: H,
    admission: ClearAdmission,
    guard: OwnedMutexGuard<()>,
) -> oneshot::Receiver<ClearOutcome> {
    let (send, recv) = oneshot::channel();
    let runtime = tokio::runtime::Handle::current();
    let end = Instant::now() + NATIVE_CLEAR_BUDGET;
    tokio::task::spawn_blocking(move || {
        let result = runtime.block_on(run_until(host, admission, end));
        drop(guard);
        let _ = send.send(result);
    });
    recv
}

async fn run_until<H: NativeClearHost>(
    mut host: H,
    admission: ClearAdmission,
    end: Instant,
) -> ClearOutcome {
    let native_end = end - FINISH_RESERVE;
    let mut changed = host.changes();
    let ready = admission == ClearAdmission::Native
        && tokio::time::timeout_at(native_end, host.prepare(native_end))
            .await
            .unwrap_or(false);
    if ready {
        let submitted = host.submit(native_end);
        if submitted != NativeClearSubmission::NotSent && Instant::now() < native_end {
            loop {
                if matches!(host.decide(false), ClearDecision::Committed(_)) {
                    break;
                }
                if tokio::time::timeout_at(native_end, changed.changed())
                    .await
                    .is_err()
                {
                    break;
                }
                if changed.has_changed().is_err() {
                    break;
                }
            }
        }
    }
    match host.decide(true) {
        ClearDecision::Committed(commit) => {
            let saved = tokio::time::timeout_at(end, host.save(commit.clone(), end))
                .await
                .unwrap_or(false);
            if saved && host.composer_empty(end) {
                ClearOutcome::Native(commit)
            } else {
                ClearOutcome::Hold(Some(commit))
            }
        }
        ClearDecision::Fallback => {
            if tokio::time::timeout_at(end, host.fallback(end))
                .await
                .unwrap_or(false)
            {
                ClearOutcome::Fallback
            } else {
                ClearOutcome::Hold(None)
            }
        }
        ClearDecision::Hold(commit) => ClearOutcome::Hold(commit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Host {
        changes: watch::Sender<u64>,
        decision: ClearDecision,
        prepare_delay: Duration,
        save_delay: Duration,
        fallback_delay: Duration,
        empty: bool,
        submits: Arc<AtomicUsize>,
        kills: Arc<AtomicUsize>,
        saved: Arc<AtomicUsize>,
        entered: Option<oneshot::Sender<()>>,
    }
    impl Host {
        fn new(decision: ClearDecision) -> Self {
            Self {
                changes: watch::channel(0).0,
                decision,
                prepare_delay: Duration::ZERO,
                save_delay: Duration::ZERO,
                fallback_delay: Duration::ZERO,
                empty: true,
                submits: Arc::new(AtomicUsize::new(0)),
                kills: Arc::new(AtomicUsize::new(0)),
                saved: Arc::new(AtomicUsize::new(0)),
                entered: None,
            }
        }
    }
    impl NativeClearHost for Host {
        fn changes(&self) -> watch::Receiver<u64> {
            self.changes.subscribe()
        }
        fn prepare(&mut self, _: Instant) -> Step<'_, bool> {
            let delay = self.prepare_delay;
            let entered = self.entered.take();
            Box::pin(async move {
                if let Some(tx) = entered {
                    let _ = tx.send(());
                }
                tokio::time::sleep(delay).await;
                true
            })
        }
        fn submit(&mut self, _: Instant) -> NativeClearSubmission {
            self.submits.fetch_add(1, Ordering::SeqCst);
            self.changes.send_replace(1);
            NativeClearSubmission::Indeterminate
        }
        fn decide(&mut self, _: bool) -> ClearDecision {
            self.decision.clone()
        }
        fn composer_empty(&mut self, _: Instant) -> bool {
            self.empty
        }
        fn save(&mut self, _: ClearCommit, _: Instant) -> Step<'_, bool> {
            let delay = self.save_delay;
            let saved = self.saved.clone();
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                saved.fetch_add(1, Ordering::SeqCst);
                true
            })
        }
        fn fallback(&mut self, _: Instant) -> Step<'_, bool> {
            self.kills.fetch_add(1, Ordering::SeqCst);
            let delay = self.fallback_delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                true
            })
        }
    }
    fn commit() -> ClearCommit {
        ClearCommit {
            seq: 2,
            session: "new".into(),
            source: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_bounds_prepare_wait_save_and_fallback() {
        let start = Instant::now();
        let mut host = Host::new(ClearDecision::Fallback);
        host.prepare_delay = Duration::from_secs(90);
        let submits = host.submits.clone();
        assert_eq!(
            run_until(host, ClearAdmission::Native, start + NATIVE_CLEAR_BUDGET).await,
            ClearOutcome::Fallback
        );
        assert!(
            Instant::now() - start <= NATIVE_CLEAR_BUDGET,
            "prepare exceeded total deadline"
        );
        assert_eq!(submits.load(Ordering::SeqCst), 0);
        let start = Instant::now();
        let host = Host::new(ClearDecision::Fallback);
        let submits = host.submits.clone();
        assert_eq!(
            run_until(host, ClearAdmission::Native, start + NATIVE_CLEAR_BUDGET).await,
            ClearOutcome::Fallback
        );
        assert!(
            Instant::now() - start <= NATIVE_CLEAR_BUDGET,
            "binding wait exceeded total deadline"
        );
        assert_eq!(submits.load(Ordering::SeqCst), 1);
        for decision in [ClearDecision::Committed(commit()), ClearDecision::Fallback] {
            let start = Instant::now();
            let mut host = Host::new(decision.clone());
            host.save_delay = Duration::from_secs(90);
            host.fallback_delay = Duration::from_secs(90);
            let kills = host.kills.clone();
            let expected = match decision.clone() {
                ClearDecision::Committed(c) => ClearOutcome::Hold(Some(c)),
                _ => ClearOutcome::Hold(None),
            };
            assert_eq!(
                run_until(host, ClearAdmission::Fallback, start + NATIVE_CLEAR_BUDGET).await,
                expected
            );
            assert!(Instant::now() - start <= NATIVE_CLEAR_BUDGET);
            assert_eq!(
                kills.load(Ordering::SeqCst),
                usize::from(matches!(decision, ClearDecision::Fallback))
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn durable_commit_never_kills_and_saves_before_empty_confirmation() {
        for empty in [true, false] {
            let mut host = Host::new(ClearDecision::Committed(commit()));
            host.empty = empty;
            let kills = host.kills.clone();
            let saved = host.saved.clone();
            let expected = if empty {
                ClearOutcome::Native(commit())
            } else {
                ClearOutcome::Hold(Some(commit()))
            };
            assert_eq!(
                run_until(
                    host,
                    ClearAdmission::Native,
                    Instant::now() + NATIVE_CLEAR_BUDGET
                )
                .await,
                expected
            );
            assert_eq!(
                kills.load(Ordering::SeqCst),
                0,
                "durable clear must forbid kill"
            );
            assert_eq!(
                saved.load(Ordering::SeqCst),
                1,
                "new selector must survive a nonempty composer"
            );
        }
        let mut host = Host::new(ClearDecision::Committed(commit()));
        host.prepare_delay = Duration::from_secs(90);
        let kills = host.kills.clone();
        assert_eq!(
            run_until(
                host,
                ClearAdmission::Native,
                Instant::now() + NATIVE_CLEAR_BUDGET
            )
            .await,
            ClearOutcome::Native(commit())
        );
        assert_eq!(kills.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn receiver_cancellation_keeps_guard_until_bounded_worker_finishes() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let guard = lock.clone().lock_owned().await;
        let mut host = Host::new(ClearDecision::Fallback);
        host.prepare_delay = Duration::from_secs(90);
        let kills = host.kills.clone();
        let (tx, entered) = oneshot::channel();
        host.entered = Some(tx);
        let receiver = start_native_clear(host, ClearAdmission::Native, guard);
        entered.await.unwrap();
        drop(receiver);
        assert!(
            lock.try_lock().is_err(),
            "cancel released transition guard early"
        );
        tokio::time::advance(NATIVE_CLEAR_BUDGET).await;
        let guard = tokio::time::timeout(Duration::from_secs(1), lock.lock())
            .await
            .expect("bounded guard release");
        assert_eq!(kills.load(Ordering::SeqCst), 1);
        drop(guard);
    }

    #[cfg(unix)]
    struct CanonicalFixture {
        _env: crate::config::TestRuntimeRootGuard,
        root: tempfile::TempDir,
        context: BindingContext,
        old: SourceId,
    }
    #[cfg(unix)]
    impl CanonicalFixture {
        fn new() -> Self {
            let env = crate::config::TestRuntimeRootGuard::new();
            let root = tempfile::tempdir().unwrap();
            binding_events::set_test_root(Some(root.path()));
            let tmux = format!("native-clear-test-{}", uuid::Uuid::new_v4());
            let context = BindingContext {
                schema: 1,
                provider: "claude".into(),
                created_at: chrono::Utc::now(),
                execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
                tmux_session: tmux.clone(),
                channel_id: Some(6577001),
                owner_runtime_root: root.path().display().to_string(),
                host: None,
                expected_native_session_id: Some("old".into()),
                launch_mode: "fresh".into(),
                provider_root: None,
            };
            let marker = tmux_common::session_temp_path(&tmux, "spawn_nonce");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(marker, &context.execution_nonce).unwrap();
            let old = SourceId {
                session_id: "old".into(),
                path: root.path().join("old.jsonl"),
                dev: 1,
                ino: 1,
            };
            let fixture = Self {
                _env: env,
                root,
                context,
                old,
            };
            fixture.record("old", BindingCause::Startup, false);
            fixture
        }
        fn record(&self, session: &str, cause: BindingCause, pending: bool) {
            use binding_events::{CauseSource, HookSignal, Proposal};
            let source = SourceId {
                session_id: session.into(),
                path: self.root.path().join(format!("{session}.jsonl")),
                dev: 1,
                ino: if session == "old" { 1 } else { 2 },
            };
            let path = source.path.display().to_string();
            let hook =
                HookSignal::from_payload("session_start", &serde_json::json!({"source":"clear"}));
            let proposal = Proposal {
                channel_id: self.context.channel_id.unwrap(),
                provider: "claude",
                tmux_session: &self.context.tmux_session,
                session_id: Some(session),
                path: &path,
                replaced: None,
                cause: CauseSource::Hook(cause),
                hook: Some(&hook),
            };
            tmux_common::with_tmux_source_authority(&self.context.tmux_session, |_| {
                if pending {
                    binding_events::record_pending(&proposal).unwrap();
                } else {
                    binding_events::record_verified(&proposal, &source).unwrap();
                }
            });
        }
        fn cleared(&self) -> crate::services::session_host::ClearedHostSession {
            use crate::services::session_host::*;
            let target = ResolvedSessionTarget {
                input: SessionTargetInput::RawName(self.context.tmux_session.clone()),
                session_key: None,
                host: TargetHost::Known {
                    kind: HostKind::Tmux,
                    source: TargetSource::SessionRecord,
                    name: self.context.tmux_session.clone(),
                },
            };
            clear_legacy_session(
                &target,
                StateChange::Automatic {
                    effect: AutomaticEffect::Clear,
                    observed: None,
                },
            )
            .unwrap()
        }
    }
    #[cfg(unix)]
    impl Drop for CanonicalFixture {
        fn drop(&mut self) {
            binding_events::forget_channel_for_tests(self.context.channel_id.unwrap());
            binding_events::set_test_root(None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn canonical_pending_source_restart_and_supersession_forbid_fallback_cut() {
        for pending in [false, true] {
            let fixture = CanonicalFixture::new();
            let waiter =
                CanonicalClearWaiter::capture(fixture.context.clone(), fixture.old.clone())
                    .unwrap();
            fixture.record("new", BindingCause::Clear, pending);
            assert!(
                waiter.changed.has_changed().unwrap(),
                "fast hook notification lost"
            );
            let marker =
                tmux_common::session_temp_path(&fixture.context.tmux_session, "spawn_nonce");
            for reload in [false, true] {
                if reload {
                    binding_events::forget_channel_for_tests(fixture.context.channel_id.unwrap());
                }
                let ClearDecision::Committed(c) = waiter.fallback_cut(&fixture.cleared()) else {
                    panic!("canonical durable commitment lost")
                };
                assert_eq!(c.session, "new");
                assert_eq!(c.source.is_none(), pending);
                assert!(
                    std::path::Path::new(&marker).exists(),
                    "durable clear invalidated execution"
                );
            }
            if pending {
                fixture.record("new", BindingCause::Clear, false);
                let ClearDecision::Committed(c) = waiter.probe() else {
                    panic!("Pending did not resolve")
                };
                assert!(c.source.is_some());
            }
            fixture.record("later", BindingCause::Resume, false);
            assert!(
                matches!(
                    waiter.fallback_cut(&fixture.cleared()),
                    ClearDecision::Hold(Some(_))
                ),
                "superseded clear must not resume stale selector or kill"
            );
            assert!(std::path::Path::new(&marker).exists());
            std::fs::remove_file(&marker).unwrap();
            assert!(matches!(waiter.probe(), ClearDecision::Hold(Some(_))));
        }
    }

    #[cfg(unix)]
    #[test]
    fn uncommitted_cut_invalidates_nonce_without_nested_lock_and_corruption_holds() {
        let fixture = CanonicalFixture::new();
        let waiter =
            CanonicalClearWaiter::capture(fixture.context.clone(), fixture.old.clone()).unwrap();
        assert_eq!(
            tmux_common::with_tmux_source_authority(&fixture.context.tmux_session, |_| waiter
                .probe()),
            ClearDecision::Hold(None)
        );
        assert_eq!(
            waiter.fallback_cut(&fixture.cleared()),
            ClearDecision::Fallback
        );
        assert_eq!(
            observe_spawn_nonce_marker(&fixture.context.tmux_session),
            SpawnNonceMarker::Absent
        );
        assert_eq!(waiter.probe(), ClearDecision::Hold(None));
        let marker = tmux_common::session_temp_path(&fixture.context.tmux_session, "spawn_nonce");
        std::fs::write(&marker, &fixture.context.execution_nonce).unwrap();
        let log = fixture
            .root
            .path()
            .join(binding_events::BINDING_EVENTS_DIR)
            .join(format!("{}.log", fixture.context.channel_id.unwrap()));
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .unwrap()
            .write_all(b"corrupt\n")
            .unwrap();
        assert_eq!(
            waiter.fallback_cut(&fixture.cleared()),
            ClearDecision::Hold(None)
        );
        assert!(std::path::Path::new(&marker).exists());
    }
}
